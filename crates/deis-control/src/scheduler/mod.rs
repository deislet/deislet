//! Cron: firing an application's scheduled handler on one node.
//!
//! A cron trigger belongs to an **application**, not to a node
//! (architecture principle 6). Which node runs a given occurrence is a
//! scheduling detail this module decides at the moment it fires; the
//! platform replicates every application to every node, so any healthy one
//! will do.
//!
//! Where the expressions come from: the manifest of the version an
//! environment currently runs (`manifest.cron`). Nothing else declares
//! them. That single source has three consequences worth stating, because
//! they are the behaviour an operator sees:
//!
//! * deploying a version that changed `cron` changes the schedule with it;
//! * rolling back restores the previous version's schedule;
//! * deleting the app, or the last deployment of an environment, stops its
//!   scheduled runs.
//!
//! Tasks are therefore derived state, keyed by `(app_id, environment)`,
//! rebuilt from the catalog on every tick and never persisted: the catalog
//! already is the record.
//!
//! One process fires. Two control planes on one database each derive the
//! same task list, so without a rule each would fire every occurrence; the
//! rule is a lease row in that database ([`lease`]), held by whichever
//! process took it first and refreshed every tick. The others keep their
//! task lists current and fire nothing until the holder stops refreshing or
//! releases the row on shutdown.

pub mod lease;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use saffron::Cron;
use serde::Serialize;
use tokio::time::{Duration, interval};
use tracing::{debug, error, info, warn};

use crate::catalog::CatalogStore;
use crate::runtime_client::{NodeError, RuntimeNodeManager, trigger::TriggerResponse};

/// Marker the node stamps on the front of `TriggerResponse.error` when the
/// application's own handler ran and failed, as opposed to this node being
/// unable to run the occurrence at all.
///
/// The other half of the pair is `deis_runtime::trigger::HANDLER_FAILED_PREFIX`.
/// The literal is spelled out again here instead of imported because this
/// crate cannot depend on deis-runtime - that is the process this one drives,
/// and it carries a whole JavaScript engine - and because the marker belongs
/// to trigger.proto rather than to either side of it. The pair is only as good
/// as the two spellings agreeing, so [`tests::a_handler_failure_is_recognised_by_its_marker`]
/// pins this one; if they ever drift, the control plane stops recognising a
/// failed handler and goes back to replaying it on every node in the fleet.
const HANDLER_FAILED_PREFIX: &str = "handler-failed: ";

/// What a task is keyed by. An application in two environments keeps two
/// independent schedules, because they run different versions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct CronKey {
    pub app_id: String,
    /// Environment name, `production` or a preview name.
    pub environment: String,
}

/// One cron expression of one deployed application.
#[derive(Debug, Clone, Serialize)]
pub struct CronTask {
    pub app_id: String,
    pub environment: String,
    /// Version whose manifest declared this expression. Kept so the log
    /// can say which version a firing schedule came from.
    pub version_id: String,
    pub cron_expr: String,
    /// Parsed form. `None` when the expression could not be parsed or can
    /// never match; the task is still kept so the problem stays visible
    /// and is reported once instead of on every tick.
    #[serde(skip)]
    schedule: Option<Cron>,
    /// Why this expression will never fire, when that is the case.
    pub unusable: Option<String>,
    pub next_run: Option<DateTime<Utc>>,
    /// When this task was last due, whether or not a node took it. The
    /// time the occurrence was due, not the time the tick fired it.
    pub last_run: Option<DateTime<Utc>>,
    /// Node the last occurrence was handed to, whether or not the handler
    /// on it succeeded. `None` after an occurrence no node accepted.
    pub last_node: Option<String>,
    /// What became of the last occurrence.
    ///
    /// Separate from `last_node` because "no node would take it" and "a node
    /// ran it and the application's handler threw" are different events with
    /// different consequences, and a single `Option<String>` said the same
    /// thing about both.
    pub last_outcome: Option<RunOutcome>,
}

impl CronTask {
    fn key(&self) -> CronKey {
        CronKey {
            app_id: self.app_id.clone(),
            environment: self.environment.clone(),
        }
    }
}

/// One occurrence that has come due, as [`CronScheduler::fire_due`] found
/// it: which task, where it sits in that key's list, and the time it was
/// due - which is what the node is told, and is not the same as the time
/// the tick noticed it.
#[derive(Debug, Clone)]
struct Due {
    key: CronKey,
    /// Position in `tasks[key]`. Advancing by position rather than by
    /// expression is what keeps two identical expressions from leaving one
    /// of them permanently due.
    index: usize,
    cron_expr: String,
    scheduled: DateTime<Utc>,
}

/// What one node's answer to a scheduled trigger means for the rest of the
/// walk over the fleet.
///
/// The three failures are not interchangeable, and only two of them are
/// answers at all: the node says which of those two it is, and the third is
/// this control plane admitting it never heard back.
#[derive(Debug, PartialEq, Eq)]
enum NodeAnswer {
    /// The handler ran here and finished.
    Ran,
    /// Nothing of the occurrence happened on this node: it does not have the
    /// application loaded, it is shutting down, the connection to it was never
    /// established. Another node may still run it.
    NotRunnableHere(String),
    /// The application's own handler ran here and threw. The occurrence has
    /// already had whatever effect the handler managed before it failed - a
    /// charge, an email, a write - so handing it to a second node would
    /// replay that effect rather than retry it.
    HandlerFailed(String),
    /// The call reached the node and this control plane stopped listening
    /// before the node answered: the deadline expired, or the connection
    /// broke with the request already on it. The handler may be running on
    /// that node right now, or may have finished; nothing here can tell.
    /// Handing the occurrence on would risk replaying it, exactly as
    /// [`NodeAnswer::HandlerFailed`] would.
    Unknown(String),
}

/// Read one node's answer to `OnScheduled`.
///
/// A failed call is not one event but two, and
/// [`NodeError::left_the_outcome_unknown`] is what tells them apart. A
/// connection that was never established means the node never saw the
/// request, so nothing of the occurrence happened there and the next node may
/// be asked. A call that went out and came back without an answer - the
/// deadline in `runtime_client` expiring is the ordinary way - means the node
/// may be running the handler at this moment, and reading that as "nothing
/// happened on this node" is what used to run one occurrence once per node in
/// the fleet.
///
/// The trigger deadline being twice the node's own execution ceiling makes
/// that second case rare. It does not make it impossible, which is why it has
/// an answer of its own rather than a comment.
fn read_answer(result: Result<TriggerResponse, NodeError>) -> NodeAnswer {
    let error = match result {
        Ok(resp) if resp.success => return NodeAnswer::Ran,
        Ok(resp) => resp.error,
        Err(e) if e.left_the_outcome_unknown() => return NodeAnswer::Unknown(e.to_string()),
        Err(e) => return NodeAnswer::NotRunnableHere(e.to_string()),
    };

    match error.strip_prefix(HANDLER_FAILED_PREFIX) {
        Some(reason) if !reason.trim().is_empty() => NodeAnswer::HandlerFailed(reason.to_string()),
        // The marker with nothing behind it still means the handler ran: what
        // is missing is only the reason, and treating that as "try elsewhere"
        // would replay the effect the marker exists to prevent.
        Some(_) => {
            NodeAnswer::HandlerFailed("the node did not say why the handler failed".to_string())
        }
        None if error.is_empty() => NodeAnswer::NotRunnableHere(
            "node reported the scheduled run as failed without saying why".to_string(),
        ),
        None => NodeAnswer::NotRunnableHere(error),
    }
}

/// What became of one occurrence once the walk over the fleet stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum RunOutcome {
    /// A node ran the handler and it finished.
    Ran { node: String },
    /// A node ran the handler and the handler itself failed. The occurrence
    /// is over: it is not handed to another node, and it is not made up later.
    HandlerFailed { node: String, error: String },
    /// A node was given the occurrence and never said what became of it. The
    /// walk stops here for the same reason [`RunOutcome::HandlerFailed`] stops
    /// it, but the record is different on purpose: after a handler failure the
    /// occurrence certainly ran, and after this one nobody knows whether it
    /// ran, half ran or never started. That is the state an operator has to go
    /// and look at the node for.
    Unknown { node: String, error: String },
    /// No node ran it at all. Every node that was tried said it could not,
    /// or could not be reached.
    NoNodeRan,
    /// This control plane did not fire it because another control plane on
    /// the same database holds the scheduler lease; that one fired it, or
    /// will report what became of it. Recorded so this process's task list
    /// moves past the occurrence and does not fire a backlog when it takes
    /// over.
    HeldElsewhere { holder: String },
}

impl RunOutcome {
    /// The node the occurrence was handed to, if any.
    fn node(&self) -> Option<&str> {
        match self {
            RunOutcome::Ran { node }
            | RunOutcome::HandlerFailed { node, .. }
            | RunOutcome::Unknown { node, .. } => Some(node),
            RunOutcome::NoNodeRan | RunOutcome::HeldElsewhere { .. } => None,
        }
    }
}

/// Outcome of one reconciliation against the catalog, for logging.
#[derive(Debug, Default, PartialEq, Eq)]
struct RefreshOutcome {
    added: usize,
    removed: usize,
    kept: usize,
}

impl RefreshOutcome {
    fn changed(&self) -> bool {
        self.added > 0 || self.removed > 0
    }
}

/// Drives every application's scheduled handlers.
pub struct CronScheduler {
    /// The catalog is the source of truth for which expressions exist.
    store: Arc<CatalogStore>,
    /// The fleet a fire is handed to.
    nodes: Arc<RuntimeNodeManager>,
    tasks: Arc<DashMap<CronKey, Vec<CronTask>>>,
    tick_interval: Duration,
    /// Rotates which node is tried first, so repeated fires spread over
    /// the fleet instead of always loading the same node.
    rotation: AtomicUsize,
    /// This process's name on the scheduler lease: process id plus a random
    /// suffix, so two instances on one machine are told apart in the log.
    holder_id: String,
    /// How long the lease stays with a holder that has stopped refreshing
    /// it. Three ticks: one tick of ordinary lateness must not hand the
    /// schedule to another process, and a holder that has been gone for
    /// three is gone.
    lease_ttl: Duration,
    /// Whether this process held the lease at the last tick, so the log
    /// says so on the change and not on every tick. `None` before the first.
    held_lease: std::sync::Mutex<Option<bool>>,
}

impl CronScheduler {
    pub fn new(
        store: Arc<CatalogStore>,
        nodes: Arc<RuntimeNodeManager>,
        tick_interval_secs: u64,
    ) -> Self {
        // A cron expression has minute resolution, so ticking faster
        // than once a minute only costs work; ticking slower would
        // start to skip occurrences.
        let tick_interval = Duration::from_secs(tick_interval_secs.clamp(1, 60));
        Self {
            store,
            nodes,
            tasks: Arc::new(DashMap::new()),
            tick_interval,
            rotation: AtomicUsize::new(0),
            holder_id: format!(
                "pid{}-{}",
                std::process::id(),
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            ),
            lease_ttl: tick_interval * 3,
            held_lease: std::sync::Mutex::new(None),
        }
    }

    /// This process's name on the scheduler lease.
    pub fn holder_id(&self) -> &str {
        &self.holder_id
    }

    /// Whether this process fires at `now`, by taking or refreshing the
    /// scheduler lease.
    ///
    /// `None` means the database could not say, which is reported and read
    /// as "not this tick": nothing is fired and nothing is advanced, so the
    /// occurrences stay due and fire on the next tick that can read the row.
    /// Firing anyway would be the double fire this lease exists to prevent,
    /// on exactly the day two processes share a struggling database.
    async fn holds_lease(&self, now: DateTime<Utc>) -> Option<lease::LeaseState> {
        let state = match lease::claim(
            &self.store.database(),
            &self.holder_id,
            now.timestamp(),
            self.lease_ttl.as_secs() as i64,
        )
        .await
        {
            Ok(state) => state,
            Err(e) => {
                error!(
                    error = %e,
                    "Could not read or write the scheduler lease, so this tick fires nothing; the occurrences that are due stay due and fire on the next tick that can reach the database"
                );
                return None;
            }
        };

        let holds = state == lease::LeaseState::Held;
        let previous = self
            .held_lease
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(holds);
        if previous != Some(holds) {
            match &state {
                lease::LeaseState::Held => info!(
                    holder = %self.holder_id,
                    lease_ttl_secs = self.lease_ttl.as_secs(),
                    "This control plane holds the cron scheduler lease and fires scheduled occurrences; another control plane on the same database would take over {}s after this one stops refreshing it, or at once when this one stops cleanly",
                    self.lease_ttl.as_secs()
                ),
                lease::LeaseState::HeldElsewhere {
                    holder,
                    refreshed_secs_ago,
                } => warn!(
                    holder = %holder,
                    refreshed_secs_ago,
                    this_instance = %self.holder_id,
                    lease_ttl_secs = self.lease_ttl.as_secs(),
                    "Another control plane holds the cron scheduler lease, so this instance fires no scheduled occurrence: it keeps its task list current and takes over {}s after that instance stops refreshing the lease. Two control planes on one database is the only way to get here; if it is not intended, stop one of them",
                    self.lease_ttl.as_secs()
                ),
            }
        }
        Some(state)
    }

    /// Give the lease up on the way out, so a second control plane takes
    /// over on its next tick rather than after the ttl.
    async fn release_lease(&self) {
        match lease::release(&self.store.database(), &self.holder_id).await {
            Ok(true) => info!(
                holder = %self.holder_id,
                "Released the cron scheduler lease; another control plane on this database fires from its next tick"
            ),
            Ok(false) => {}
            Err(e) => warn!(
                error = %e,
                lease_ttl_secs = self.lease_ttl.as_secs(),
                "Could not release the cron scheduler lease; another control plane on this database takes over {}s after this one's last refresh instead of at once",
                self.lease_ttl.as_secs()
            ),
        }
    }

    /// Start the tick loop.
    ///
    /// It stops when `shutdown` is triggered, and the returned handle is what
    /// the caller waits on to know that it has. The stop is cooperative on
    /// purpose: this loop calls nodes, and aborting the task can land between
    /// sending a trigger and reading its answer - the occurrence then runs on
    /// a node while this process records nothing about it, so the next
    /// instance can fire it again. Waiting for the loop to reach the top of a
    /// tick costs at most one in-flight trigger's deadline and leaves the
    /// records straight.
    pub async fn start(
        self: Arc<Self>,
        shutdown: crate::shutdown::Shutdown,
    ) -> tokio::task::JoinHandle<()> {
        // Load once before the first tick so a task that is due right away
        // is not delayed by a whole interval.
        self.refresh().await;

        let scheduler = Arc::clone(&self);
        let handle = tokio::spawn(async move {
            let mut ticker = interval(scheduler.tick_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.wait() => {
                        info!(
                            tasks = scheduler.task_count(),
                            "Cron scheduler stopped for shutdown; no occurrence fires from this instance any more, and an occurrence that comes due while nothing is running is skipped rather than made up later"
                        );
                        scheduler.release_lease().await;
                        return;
                    }
                    _ = ticker.tick() => {}
                }
                scheduler.refresh().await;
                scheduler.fire_due(Utc::now(), &shutdown).await;
            }
        });

        info!(
            interval_secs = self.tick_interval.as_secs(),
            tasks = self.task_count(),
            "Cron scheduler started; expressions come from the manifest of each environment's deployed version"
        );
        handle
    }

    /// Every task currently known, for tests and future read APIs.
    pub fn list_tasks(&self) -> Vec<CronTask> {
        self.tasks
            .iter()
            .flat_map(|entry| entry.value().clone())
            .collect()
    }

    /// Tasks of one application environment.
    pub fn tasks_for(&self, app_id: &str, environment: &str) -> Vec<CronTask> {
        self.tasks
            .get(&CronKey {
                app_id: app_id.to_string(),
                environment: environment.to_string(),
            })
            .map(|entry| entry.value().clone())
            .unwrap_or_default()
    }

    pub fn task_count(&self) -> usize {
        self.tasks.iter().map(|entry| entry.value().len()).sum()
    }

    /// Rebuild the task set from the catalog.
    ///
    /// Existing tasks keep their schedule state: an expression that did not
    /// change must not have its next occurrence pushed back by an unrelated
    /// deploy, which is what re-registering it from scratch would do.
    async fn refresh(&self) {
        let deployed = match self.store.list_deployed_versions().await {
            Ok(deployed) => deployed,
            Err(e) => {
                warn!(
                    error = %e,
                    "Could not read the deployed versions from the catalog; the cron task list is left as it was, so it may fire a schedule that has since been deployed away"
                );
                return;
            }
        };

        let now = Utc::now();
        let mut outcome = RefreshOutcome::default();
        let mut seen: HashSet<CronKey> = HashSet::with_capacity(deployed.len());

        for current in deployed {
            let key = CronKey {
                app_id: current.app_id.clone(),
                environment: current.environment.clone(),
            };
            seen.insert(key.clone());

            let expressions = match deis_bundle::Manifest::parse(&current.manifest) {
                Ok(manifest) => manifest.cron,
                Err(e) => {
                    // Not "declares no cron": the declaration could not be
                    // read at all, which is a different thing to fix.
                    let dropped = self.tasks.remove(&key).map(|(_, v)| v.len()).unwrap_or(0);
                    outcome.removed += dropped;
                    warn!(
                        app_id = %current.app_id,
                        environment = %current.environment,
                        version_id = %current.version_id,
                        dropped,
                        error = %e,
                        "Deployed version's manifest does not parse, so its cron expressions cannot be read; this environment's scheduled runs stop until a readable version is deployed"
                    );
                    continue;
                }
            };

            if expressions.is_empty() {
                if let Some((_, previous)) = self.tasks.remove(&key) {
                    outcome.removed += previous.len();
                    info!(
                        app_id = %current.app_id,
                        environment = %current.environment,
                        version_id = %current.version_id,
                        dropped = previous.len(),
                        "The deployed version declares no cron; these scheduled runs stop here"
                    );
                }
                continue;
            }

            let mut existing = self.tasks.remove(&key).map(|(_, v)| v).unwrap_or_default();
            let mut rebuilt = Vec::with_capacity(expressions.len());
            let mut kept_expressions: HashSet<String> = HashSet::with_capacity(expressions.len());
            for expr in expressions {
                // The same expression twice would be two tasks that both
                // come due at the same instant, so the handler would run
                // twice per occurrence - and only one of the two would ever
                // have its next occurrence advanced, leaving the other due
                // for ever and firing on every single tick.
                if !kept_expressions.insert(expr.clone()) {
                    warn!(
                        app_id = %current.app_id,
                        environment = %current.environment,
                        version_id = %current.version_id,
                        cron = %expr,
                        "The deployed manifest declares this cron expression more than once; the repeats are ignored, so the handler runs once per occurrence and not once per copy"
                    );
                    continue;
                }
                match existing.iter().position(|t| t.cron_expr == expr) {
                    Some(index) => {
                        let mut task = existing.remove(index);
                        task.version_id = current.version_id.clone();
                        outcome.kept += 1;
                        rebuilt.push(task);
                    }
                    None => {
                        outcome.added += 1;
                        rebuilt.push(new_task(&current, expr, now));
                    }
                }
            }
            outcome.removed += existing.len();
            for gone in existing {
                info!(
                    app_id = %gone.app_id,
                    environment = %gone.environment,
                    cron = %gone.cron_expr,
                    "The deployed version no longer declares this cron; it will not fire again"
                );
            }
            self.tasks.insert(key, rebuilt);
        }

        // Environments that lost their last deployment, or apps that were
        // deleted, are simply no longer in the catalog answer.
        let stale: Vec<CronKey> = self
            .tasks
            .iter()
            .map(|entry| entry.key().clone())
            .filter(|key| !seen.contains(key))
            .collect();
        for key in stale {
            if let Some((_, dropped)) = self.tasks.remove(&key) {
                outcome.removed += dropped.len();
                info!(
                    app_id = %key.app_id,
                    environment = %key.environment,
                    dropped = dropped.len(),
                    "This environment is no longer deployed; its scheduled runs stop here"
                );
            }
        }

        if outcome.changed() {
            info!(
                added = outcome.added,
                removed = outcome.removed,
                kept = outcome.kept,
                total = self.task_count(),
                "Cron task list rebuilt from the catalog"
            );
        } else {
            debug!(tasks = outcome.kept, "Cron task list unchanged");
        }
    }

    /// Fire everything that came due, one node per occurrence - if this
    /// process holds the scheduler lease.
    ///
    /// `shutdown` is checked between occurrences, not inside one: a trigger
    /// that is already in flight is seen through to its answer so the run is
    /// recorded, while the occurrences behind it are left for the next
    /// instance to find due.
    async fn fire_due(&self, now: DateTime<Utc>, shutdown: &crate::shutdown::Shutdown) {
        // Decided before anything is collected, and on every tick even when
        // nothing is due: the claim is also the refresh that keeps the lease
        // from expiring under a holder with an empty schedule.
        let elsewhere = match self.holds_lease(now).await {
            Some(lease::LeaseState::Held) => None,
            Some(lease::LeaseState::HeldElsewhere { holder, .. }) => Some(holder),
            None => return,
        };

        // Collect first, then call. Holding a DashMap guard across an await
        // would block every other reader of the shard for the length of a
        // gRPC call.
        //
        // The position in the list travels with each item, not just the
        // expression: it is what the occurrence is recorded against
        // afterwards, so a task can never be fired without being advanced.
        let due: Vec<Due> = self
            .tasks
            .iter()
            .flat_map(|entry| {
                entry
                    .value()
                    .iter()
                    .enumerate()
                    .filter_map(|(index, task)| {
                        let scheduled = task.next_run.filter(|next| *next <= now)?;
                        Some(Due {
                            key: task.key(),
                            index,
                            cron_expr: task.cron_expr.clone(),
                            scheduled,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        if due.is_empty() {
            return;
        }

        if let Some(holder) = elsewhere {
            // Not fired here, and not left due either: a task left in the
            // past would fire the moment this process took the lease over,
            // as a backlog of every occurrence the other process already ran.
            debug!(
                occurrences = due.len(),
                holder = %holder,
                "Occurrences came due while another control plane holds the scheduler lease; they are recorded as fired elsewhere and this instance moves past them"
            );
            for item in due {
                self.record_run(
                    &item,
                    now,
                    RunOutcome::HeldElsewhere {
                        holder: holder.clone(),
                    },
                );
            }
            return;
        }

        let total = due.len();
        for (fired, item) in due.into_iter().enumerate() {
            if shutdown.is_triggered() {
                warn!(
                    fired,
                    skipped = total - fired,
                    "Shutdown was signalled part-way through this tick; the remaining occurrences do not run here and are not made up later - the next instance fires them only if they are still due when it ticks"
                );
                return;
            }
            let outcome = self.trigger_once(&item, now).await;
            self.record_run(&item, now, outcome);
        }
    }

    /// Hand one occurrence to the fleet, over this control plane's own
    /// transport.
    async fn trigger_once(&self, due: &Due, now: DateTime<Utc>) -> RunOutcome {
        self.fire_on_fleet(due, now, |node| {
            let key = due.key.clone();
            let cron = due.cron_expr.clone();
            let scheduled = due.scheduled;
            async move {
                self.nodes
                    .trigger_scheduled(&node, &key.app_id, &key.environment, &cron, scheduled)
                    .await
            }
        })
        .await
    }

    /// Walk the fleet for one occurrence, stopping at the first node that
    /// answers something final.
    ///
    /// Three of the four answers end the walk. A node that ran the handler
    /// ends it because the occurrence happened; a node whose *handler* failed
    /// ends it because the occurrence also happened - the handler had
    /// whatever effect it managed before it threw, and asking a second node
    /// would replay that effect, not retry it; and a node that was given the
    /// occurrence and never answered ends it because nothing here can rule out
    /// that the handler is running there right now. Only "this node cannot run
    /// it" moves on, because that is the one answer which says nothing of the
    /// occurrence has happened yet.
    ///
    /// The call is a parameter so the decision above can be tested without a
    /// fleet of live nodes: getting it wrong is not a failure anything would
    /// notice, it is one due occurrence charging a card once per node.
    async fn fire_on_fleet<F, Fut>(&self, due: &Due, now: DateTime<Utc>, mut call: F) -> RunOutcome
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<TriggerResponse, NodeError>>,
    {
        let Due {
            key,
            cron_expr,
            scheduled,
            ..
        } = due;
        let cron_expr = cron_expr.as_str();
        // The node is told when the occurrence was *due*, not when this
        // tick noticed it. A handler that uses scheduled_time as an
        // idempotency key would otherwise see a value that moves by up to
        // one tick interval between runs of the same occurrence, and two
        // control planes ticking at different offsets would disagree about
        // the same occurrence entirely.
        let lateness = (now - *scheduled).num_seconds();
        if lateness >= self.tick_interval.as_secs() as i64 {
            debug!(
                app_id = %key.app_id,
                environment = %key.environment,
                cron = %cron_expr,
                due = %scheduled,
                late_seconds = lateness,
                "Firing an occurrence later than one tick after it was due; the node is told the time it was due, not now"
            );
        }
        let rotation = self.rotation.fetch_add(1, Ordering::Relaxed);
        let candidates = self.nodes.call_order(rotation);
        if candidates.is_empty() {
            warn!(
                app_id = %key.app_id,
                environment = %key.environment,
                cron = %cron_expr,
                "No runtime node is configured, so this scheduled run does not happen at all; the occurrence is skipped, not queued"
            );
            return RunOutcome::NoNodeRan;
        }

        let mut failures: Vec<String> = Vec::new();
        for (tried, node) in candidates.iter().enumerate() {
            let error = match read_answer(call(node.clone()).await) {
                NodeAnswer::Ran => {
                    info!(
                        app_id = %key.app_id,
                        environment = %key.environment,
                        cron = %cron_expr,
                        node = %node,
                        healthy = self.nodes.is_healthy(node),
                        nodes_tried = tried + 1,
                        "Scheduled run executed on one node"
                    );
                    return RunOutcome::Ran { node: node.clone() };
                }
                NodeAnswer::HandlerFailed(error) => {
                    // Not "try the next node". The handler ran on this one and
                    // threw part-way through, so whatever it had already done -
                    // a charge, an email, a write - is done. Every remaining
                    // node would repeat that, once each, for a single due
                    // occurrence.
                    warn!(
                        app_id = %key.app_id,
                        environment = %key.environment,
                        cron = %cron_expr,
                        node = %node,
                        error = %error,
                        due = %scheduled,
                        nodes_tried = tried + 1,
                        nodes_not_tried = candidates.len() - tried - 1,
                        "The application's own scheduled handler ran on this node and failed. The occurrence is over: the remaining {} node(s) of the fleet are deliberately not asked, because they would replay whatever the handler managed before it threw rather than retry it. It is not made up later either; the next occurrence fires on schedule",
                        candidates.len() - tried - 1
                    );
                    return RunOutcome::HandlerFailed {
                        node: node.clone(),
                        error,
                    };
                }
                NodeAnswer::Unknown(error) => {
                    // Also not "try the next node", and for a worse reason
                    // than a handler failure: the node was handed the
                    // occurrence and this control plane stopped listening
                    // before it answered, so the handler may be running there
                    // at this moment. Every remaining node would start a
                    // second copy of it.
                    error!(
                        app_id = %key.app_id,
                        environment = %key.environment,
                        cron = %cron_expr,
                        node = %node,
                        error = %error,
                        due = %scheduled,
                        nodes_tried = tried + 1,
                        nodes_not_tried = candidates.len() - tried - 1,
                        "This node was given the scheduled occurrence and never answered, so it is not known whether the handler ran, ran half way or never started. The remaining {} node(s) of the fleet are deliberately not asked, because a handler that is still running there would then be running twice; and the occurrence is not made up later. Check that node's log for this occurrence before assuming it was missed",
                        candidates.len() - tried - 1
                    );
                    return RunOutcome::Unknown {
                        node: node.clone(),
                        error,
                    };
                }
                NodeAnswer::NotRunnableHere(error) => error,
            };
            warn!(
                app_id = %key.app_id,
                environment = %key.environment,
                cron = %cron_expr,
                node = %node,
                error = %error,
                "Node could not run this scheduled occurrence, so nothing of it happened there; trying the next node"
            );
            failures.push(format!("{node}: {error}"));
        }

        warn!(
            app_id = %key.app_id,
            environment = %key.environment,
            cron = %cron_expr,
            nodes_tried = candidates.len(),
            failures = ?failures,
            "Scheduled run skipped: none of the {} node(s) tried could run it. This occurrence does not run and is not retried; the next one fires on schedule",
            candidates.len()
        );
        RunOutcome::NoNodeRan
    }

    /// Record an occurrence and compute the next one.
    ///
    /// `next_run` advances whether or not a node took the fire. Leaving it
    /// in the past would re-fire the same occurrence on every tick, which
    /// turns one unreachable fleet into a flood of duplicate runs once it
    /// comes back.
    ///
    /// The task is found by the position [`Self::fire_due`] read it from,
    /// not by searching for its expression: a search advances whichever
    /// copy comes first, so any list that ever held two identical
    /// expressions would leave the second one permanently due.
    ///
    /// The next occurrence is computed from `now` rather than from the time
    /// the fire was due, so a tick that ran late does not immediately find
    /// the same task due again.
    fn record_run(&self, due: &Due, now: DateTime<Utc>, outcome: RunOutcome) {
        let Some(mut entry) = self.tasks.get_mut(&due.key) else {
            return;
        };
        let Some(task) = entry
            .value_mut()
            .get_mut(due.index)
            .filter(|task| task.cron_expr == due.cron_expr)
        else {
            // The list was rebuilt under this fire - the version was
            // deployed away between collecting and firing - so there is
            // nothing left to advance.
            debug!(
                app_id = %due.key.app_id,
                environment = %due.key.environment,
                cron = %due.cron_expr,
                "The task list changed while this occurrence was firing, so nothing was recorded against it"
            );
            return;
        };
        task.last_run = Some(due.scheduled);
        task.last_node = outcome.node().map(str::to_string);
        task.last_outcome = Some(outcome);
        task.next_run = task.schedule.as_ref().and_then(|cron| cron.next_after(now));
        debug!(
            app_id = %due.key.app_id,
            environment = %due.key.environment,
            cron = %due.cron_expr,
            was_due = %due.scheduled,
            outcome = ?task.last_outcome,
            next_run = ?task.next_run,
            "Scheduled the next occurrence"
        );
    }
}

/// Parse one manifest expression into a task, reporting an unusable one
/// once, here, instead of on every tick.
fn new_task(
    current: &crate::catalog::DeployedVersion,
    cron_expr: String,
    now: DateTime<Utc>,
) -> CronTask {
    let mut task = CronTask {
        app_id: current.app_id.clone(),
        environment: current.environment.clone(),
        version_id: current.version_id.clone(),
        cron_expr,
        schedule: None,
        unusable: None,
        next_run: None,
        last_run: None,
        last_node: None,
        last_outcome: None,
    };

    match task.cron_expr.parse::<Cron>() {
        Ok(cron) => match cron.next_after(now) {
            Some(next) => {
                info!(
                    app_id = %task.app_id,
                    environment = %task.environment,
                    version_id = %task.version_id,
                    cron = %task.cron_expr,
                    next_run = %next,
                    "Cron registered from the deployed manifest"
                );
                task.schedule = Some(cron);
                task.next_run = Some(next);
            }
            None => {
                let reason =
                    "the expression parses but matches no date that will ever occur".to_string();
                warn!(
                    app_id = %task.app_id,
                    environment = %task.environment,
                    version_id = %task.version_id,
                    cron = %task.cron_expr,
                    "This cron expression never matches any time, so the handler it names will never run; fix the manifest and deploy again"
                );
                task.unusable = Some(reason);
            }
        },
        Err(_) => {
            // The parser's own error carries no detail worth printing
            // ("failed to parse"), so the reason is spelled out here
            // instead: it is what an operator has to act on.
            let reason = "not a valid five-field cron expression \
                 (minute hour day-of-month month day-of-week)"
                .to_string();
            warn!(
                app_id = %task.app_id,
                environment = %task.environment,
                version_id = %task.version_id,
                cron = %task.cron_expr,
                "Cron expression in the deployed manifest does not parse, so this scheduled handler never runs; five fields are expected, for example \"0 */6 * * *\""
            );
            task.unusable = Some(reason);
        }
    }

    task
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CatalogStore, Environment};
    use crate::database::Database;

    async fn scheduler() -> (Arc<CronScheduler>, Arc<CatalogStore>) {
        scheduler_with_nodes(&[]).await
    }

    /// A scheduler over a named fleet. The node entries are never dialled by
    /// these tests - [`CronScheduler::fire_on_fleet`] takes the call as a
    /// parameter - they only have to exist so that `call_order` returns them.
    async fn scheduler_with_nodes(ids: &[&str]) -> (Arc<CronScheduler>, Arc<CatalogStore>) {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        scheduler_over(database, ids)
    }

    fn scheduler_over(
        database: Arc<Database>,
        ids: &[&str],
    ) -> (Arc<CronScheduler>, Arc<CatalogStore>) {
        let store = Arc::new(CatalogStore::new(database));
        let nodes = Arc::new(RuntimeNodeManager::new(
            ids.iter()
                .map(|id| crate::config::RuntimeNodeConfig {
                    id: (*id).to_string(),
                    // Nothing is ever sent here; see above.
                    addr: "http://127.0.0.1:1".to_string(),
                })
                .collect(),
            15,
        ));
        (
            Arc::new(CronScheduler::new(Arc::clone(&store), nodes, 60)),
            store,
        )
    }

    /// The occurrence `fire_due` would have collected for the first task of
    /// `demo`'s production environment.
    fn due_now(scheduled: DateTime<Utc>) -> Due {
        Due {
            key: CronKey {
                app_id: "demo".to_string(),
                environment: "production".to_string(),
            },
            index: 0,
            cron_expr: "*/5 * * * *".to_string(),
            scheduled,
        }
    }

    fn answer(success: bool, error: &str) -> TriggerResponse {
        TriggerResponse {
            success,
            failed_message_ids: Vec::new(),
            error: error.to_string(),
        }
    }

    fn manifest(cron: &[&str]) -> String {
        let list = cron
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"schema":1,"app":"demo","entry":"worker/index.js","cron":[{list}]}}"#)
    }

    /// Create an app plus environment and deploy a version whose manifest
    /// declares `cron`. Returns the environment for further deploys.
    async fn deploy(store: &CatalogStore, env: Option<Environment>, cron: &[&str]) -> Environment {
        let env = match env {
            Some(env) => env,
            None => {
                store.create_app("demo").await.unwrap();
                store
                    .create_environment("demo", "production")
                    .await
                    .unwrap()
            }
        };
        let sha = format!("{:064x}", cron.len() * 7 + cron.concat().len());
        let (version, _) = store
            .create_version("demo", &sha, "/tmp/demo.zip", &manifest(cron))
            .await
            .unwrap();
        store.deploy(&env, &version.id, None).await.unwrap();
        env
    }

    #[tokio::test]
    async fn tasks_come_from_the_deployed_manifest_and_follow_it() {
        let (scheduler, store) = scheduler().await;
        let env = deploy(&store, None, &["0 */6 * * *", "30 2 * * *"]).await;

        scheduler.refresh().await;
        let tasks = scheduler.tasks_for("demo", "production");
        assert_eq!(tasks.len(), 2);
        assert_eq!(scheduler.list_tasks().len(), 2);
        assert!(tasks.iter().all(|t| t.next_run.is_some()));
        assert!(tasks.iter().all(|t| t.unusable.is_none()));

        // The next occurrence of an expression that did not change must
        // survive a refresh; otherwise every deploy would push it back.
        let first_next = scheduler.tasks_for("demo", "production")[0].next_run;
        let env = deploy(&store, Some(env), &["0 */6 * * *"]).await;
        scheduler.refresh().await;
        let tasks = scheduler.tasks_for("demo", "production");
        assert_eq!(tasks.len(), 1, "the dropped expression must be gone");
        assert_eq!(tasks[0].cron_expr, "0 */6 * * *");
        assert_eq!(tasks[0].next_run, first_next);

        // A version without cron clears the schedule.
        deploy(&store, Some(env), &[]).await;
        scheduler.refresh().await;
        assert_eq!(scheduler.task_count(), 0);
    }

    /// The r9 finding: two control planes on one database each fired every
    /// occurrence. Now one holds the lease and fires; the other keeps its
    /// list current, records the occurrence as fired elsewhere, and fires
    /// only once the holder releases the lease or stops refreshing it.
    #[tokio::test]
    async fn two_control_planes_on_one_database_fire_each_occurrence_once() {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        // No nodes: a fired occurrence is recorded as `NoNodeRan`, which is
        // enough to tell "this instance fired" from "this instance stood
        // aside" without dialling anything.
        let (first, store) = scheduler_over(Arc::clone(&database), &[]);
        let (second, _) = scheduler_over(Arc::clone(&database), &[]);
        deploy(&store, None, &["* * * * *"]).await;
        first.refresh().await;
        second.refresh().await;
        let shutdown = crate::shutdown::Shutdown::new();

        let outcome = |scheduler: &CronScheduler| {
            scheduler.tasks_for("demo", "production")[0]
                .last_outcome
                .clone()
        };
        let due = |scheduler: &CronScheduler| scheduler.tasks_for("demo", "production")[0].next_run;

        // Both tick on the same due occurrence. The first to claim fires.
        let t1 = due(&first).unwrap() + chrono::Duration::seconds(1);
        first.fire_due(t1, &shutdown).await;
        second.fire_due(t1, &shutdown).await;
        assert_eq!(outcome(&first), Some(RunOutcome::NoNodeRan));
        assert_eq!(
            outcome(&second),
            Some(RunOutcome::HeldElsewhere {
                holder: first.holder_id().to_string()
            })
        );
        assert!(
            due(&second).unwrap() > t1,
            "the stand-aside instance moves past the occurrence rather than keeping it due"
        );

        // The holder keeps the lease tick after tick, and the other keeps
        // standing aside - for longer than the ttl, because the holder
        // refreshes.
        let mut t = t1;
        for _ in 0..5 {
            t = due(&first).unwrap() + chrono::Duration::seconds(1);
            first.fire_due(t, &shutdown).await;
            second.fire_due(t, &shutdown).await;
            assert_eq!(outcome(&first), Some(RunOutcome::NoNodeRan));
            assert!(matches!(
                outcome(&second),
                Some(RunOutcome::HeldElsewhere { .. })
            ));
        }
        assert!(
            t - t1 > chrono::Duration::from_std(first.lease_ttl).unwrap(),
            "the run outlasted the ttl and the holder kept the lease by refreshing it"
        );

        // The holder stops refreshing without releasing - crashed, or cut
        // off. Inside the ttl the other still stands aside; past it, it
        // takes over.
        let t_inside = due(&second).unwrap() + chrono::Duration::seconds(1);
        second.fire_due(t_inside, &shutdown).await;
        assert!(matches!(
            outcome(&second),
            Some(RunOutcome::HeldElsewhere { .. })
        ));
        let t_past = t
            + chrono::Duration::from_std(first.lease_ttl).unwrap()
            + chrono::Duration::seconds(61);
        second.fire_due(t_past, &shutdown).await;
        assert_eq!(
            outcome(&second),
            Some(RunOutcome::NoNodeRan),
            "the lease passed to the instance that is still ticking"
        );
        // And the old holder, if it comes back, is the one standing aside.
        let t_back = due(&first).unwrap().max(t_past) + chrono::Duration::seconds(1);
        first.fire_due(t_back, &shutdown).await;
        assert!(matches!(
            outcome(&first),
            Some(RunOutcome::HeldElsewhere { ref holder }) if holder == second.holder_id()
        ));

        // A clean stop hands over at once.
        second.release_lease().await;
        let t_after = due(&first).unwrap().max(t_back) + chrono::Duration::seconds(1);
        first.fire_due(t_after, &shutdown).await;
        assert_eq!(outcome(&first), Some(RunOutcome::NoNodeRan));
    }

    #[tokio::test]
    async fn a_rollback_restores_the_previous_schedule() {
        let (scheduler, store) = scheduler().await;
        let env = deploy(&store, None, &["0 */6 * * *"]).await;
        deploy(&store, Some(env.clone()), &[]).await;
        scheduler.refresh().await;
        assert_eq!(scheduler.task_count(), 0);

        // Rolling back is just another deployment pointer at the older
        // version, so its cron comes back with it.
        let versions = store.list_versions("demo").await.unwrap();
        let with_cron = versions
            .iter()
            .find(|v| v.manifest.contains("0 */6 * * *"))
            .unwrap();
        store.deploy(&env, &with_cron.id, None).await.unwrap();
        scheduler.refresh().await;
        assert_eq!(scheduler.task_count(), 1);
    }

    #[tokio::test]
    async fn deleting_the_app_stops_its_scheduled_runs() {
        let (scheduler, store) = scheduler().await;
        deploy(&store, None, &["0 */6 * * *"]).await;
        scheduler.refresh().await;
        assert_eq!(scheduler.task_count(), 1);

        store.delete_app("demo").await.unwrap();
        scheduler.refresh().await;
        assert_eq!(scheduler.task_count(), 0);
    }

    #[tokio::test]
    async fn an_unusable_expression_is_kept_visible_and_never_fires() {
        let (scheduler, store) = scheduler().await;
        // Not five fields, and a date that never occurs.
        deploy(&store, None, &["*/5", "* * 31 2 *"]).await;
        scheduler.refresh().await;

        let tasks = scheduler.tasks_for("demo", "production");
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| t.next_run.is_none()));
        assert!(tasks.iter().all(|t| t.unusable.is_some()));

        // Nothing is due, because nothing has a next occurrence.
        scheduler
            .fire_due(Utc::now(), &crate::shutdown::Shutdown::new())
            .await;
        assert!(
            scheduler
                .tasks_for("demo", "production")
                .iter()
                .all(|t| t.last_run.is_none())
        );
    }

    /// Pull every occurrence of one environment into the past, the way a
    /// tick that arrives after the due time finds them.
    fn make_due(scheduler: &CronScheduler, at: DateTime<Utc>) {
        let key = CronKey {
            app_id: "demo".to_string(),
            environment: "production".to_string(),
        };
        scheduler
            .tasks
            .get_mut(&key)
            .unwrap()
            .value_mut()
            .iter_mut()
            .for_each(|task| task.next_run = Some(at));
    }

    /// An occurrence nobody could run is skipped, not repeated: the task
    /// must move on to its next occurrence anyway.
    #[tokio::test]
    async fn a_due_task_advances_even_when_no_node_takes_it() {
        let (scheduler, store) = scheduler().await;
        deploy(&store, None, &["*/5 * * * *"]).await;
        scheduler.refresh().await;

        let now = Utc::now();
        let was_due = now - chrono::Duration::minutes(1);
        make_due(&scheduler, was_due);

        scheduler
            .fire_due(now, &crate::shutdown::Shutdown::new())
            .await;

        let task = &scheduler.tasks_for("demo", "production")[0];
        // The occurrence is recorded at the time it was due, not at the
        // time the tick got round to it.
        assert_eq!(task.last_run, Some(was_due));
        assert!(task.last_node.is_none(), "no node was configured");
        assert!(
            task.next_run.is_some_and(|next| next > now),
            "the next occurrence must be in the future, or the fire repeats every tick"
        );
    }

    /// The marker the node stamps on a failed handler, spelled exactly as
    /// `deis_runtime::trigger::HANDLER_FAILED_PREFIX` spells it.
    ///
    /// The two crates cannot share the constant, so this test is the joint.
    /// If it ever has to be changed to make the suite pass, the wire contract
    /// has moved and the change belongs in trigger.proto, on both sides at
    /// once - not here alone, because a control plane that stops recognising
    /// the marker silently goes back to replaying failed handlers across the
    /// whole fleet.
    #[test]
    fn a_handler_failure_is_recognised_by_its_marker() {
        assert_eq!(HANDLER_FAILED_PREFIX, "handler-failed: ");

        assert_eq!(
            read_answer(Ok(answer(
                false,
                "handler-failed: TypeError: x is not a function"
            ))),
            NodeAnswer::HandlerFailed("TypeError: x is not a function".to_string())
        );
        // The marker with nothing behind it is still a handler that ran.
        assert!(matches!(
            read_answer(Ok(answer(false, "handler-failed: "))),
            NodeAnswer::HandlerFailed(_)
        ));
        assert_eq!(read_answer(Ok(answer(true, ""))), NodeAnswer::Ran);
        assert_eq!(
            read_answer(Ok(answer(false, "app demo is not loaded here"))),
            NodeAnswer::NotRunnableHere("app demo is not loaded here".to_string())
        );
        // A node that answers "failed" and says nothing must not be read as a
        // failed handler, or an occurrence that never ran anywhere would stop
        // being tried elsewhere.
        assert!(matches!(
            read_answer(Ok(answer(false, ""))),
            NodeAnswer::NotRunnableHere(_)
        ));
        assert!(matches!(
            read_answer(Err(NodeError::Transport("connect refused".to_string()))),
            NodeAnswer::NotRunnableHere(_)
        ));
    }

    /// One due occurrence whose handler throws must run on **one** node.
    ///
    /// The walk used to treat "the application's handler threw" exactly like
    /// "this node cannot run it", so it carried on down the fleet: a single
    /// occurrence executed the handler on every node in turn, replaying
    /// whatever it had already done - a charge, an email, a write - once per
    /// node.
    #[tokio::test]
    async fn a_handler_that_throws_is_not_replayed_on_the_rest_of_the_fleet() {
        let (scheduler, store) = scheduler_with_nodes(&["node-a", "node-b", "node-c"]).await;
        deploy(&store, None, &["*/5 * * * *"]).await;
        scheduler.refresh().await;

        let now = Utc::now();
        let asked = std::sync::Mutex::new(Vec::<String>::new());
        let outcome = scheduler
            .fire_on_fleet(&due_now(now), now, |node| {
                asked.lock().unwrap().push(node.clone());
                async move { Ok(answer(false, "handler-failed: charge already attempted")) }
            })
            .await;

        assert_eq!(
            asked.into_inner().unwrap(),
            ["node-a"],
            "the occurrence had already had its effect on node-a; every further node replays it"
        );
        assert_eq!(
            outcome,
            RunOutcome::HandlerFailed {
                node: "node-a".to_string(),
                error: "charge already attempted".to_string(),
            }
        );
    }

    /// The other half of the same distinction: a node that could not run it
    /// at all has had no effect, so the fleet walk must carry on.
    #[tokio::test]
    async fn a_node_that_cannot_run_it_hands_the_occurrence_on() {
        let (scheduler, store) = scheduler_with_nodes(&["node-a", "node-b", "node-c"]).await;
        deploy(&store, None, &["*/5 * * * *"]).await;
        scheduler.refresh().await;

        let now = Utc::now();
        let asked = std::sync::Mutex::new(Vec::<String>::new());
        let outcome = scheduler
            .fire_on_fleet(&due_now(now), now, |node| {
                asked.lock().unwrap().push(node.clone());
                let refuse = node != "node-b";
                async move {
                    if refuse {
                        Ok(answer(false, "app demo is not loaded here"))
                    } else {
                        Ok(answer(true, ""))
                    }
                }
            })
            .await;

        assert_eq!(asked.into_inner().unwrap(), ["node-a", "node-b"]);
        assert_eq!(
            outcome,
            RunOutcome::Ran {
                node: "node-b".to_string()
            }
        );
    }

    /// What is recorded against the task has to keep the two apart as well:
    /// "a node ran it and the handler threw" is not "nobody would take it".
    #[tokio::test]
    async fn the_task_records_which_of_the_two_failures_happened() {
        let (scheduler, store) = scheduler_with_nodes(&["node-a"]).await;
        deploy(&store, None, &["*/5 * * * *"]).await;
        scheduler.refresh().await;

        let now = Utc::now();
        let was_due = now - chrono::Duration::minutes(1);
        make_due(&scheduler, was_due);
        let due = due_now(was_due);

        scheduler.record_run(
            &due,
            now,
            RunOutcome::HandlerFailed {
                node: "node-a".to_string(),
                error: "boom".to_string(),
            },
        );
        let task = &scheduler.tasks_for("demo", "production")[0];
        assert_eq!(task.last_node.as_deref(), Some("node-a"));
        assert!(matches!(
            task.last_outcome,
            Some(RunOutcome::HandlerFailed { .. })
        ));

        scheduler.record_run(&due, now, RunOutcome::NoNodeRan);
        let task = &scheduler.tasks_for("demo", "production")[0];
        assert!(task.last_node.is_none());
        assert_eq!(task.last_outcome, Some(RunOutcome::NoNodeRan));
    }

    /// A manifest that repeats a cron line - one copy-paste - used to
    /// register two tasks that both came due at the same instant, so the
    /// handler ran twice; and since only the first was ever advanced, the
    /// second stayed due and fired again on every single tick.
    #[tokio::test]
    async fn a_repeated_cron_expression_becomes_one_task() {
        let (scheduler, store) = scheduler().await;
        deploy(&store, None, &["*/5 * * * *", "*/5 * * * *", "30 2 * * *"]).await;
        scheduler.refresh().await;

        let tasks = scheduler.tasks_for("demo", "production");
        assert_eq!(tasks.len(), 2, "the repeat must not become a second task");
        let mut expressions: Vec<&str> = tasks.iter().map(|t| t.cron_expr.as_str()).collect();
        expressions.sort_unstable();
        assert_eq!(expressions, ["*/5 * * * *", "30 2 * * *"]);
    }

    /// Every task that fires must be advanced, whatever its position in
    /// the list: advancing by a search for the expression would leave any
    /// later duplicate permanently due.
    #[tokio::test]
    async fn every_fired_task_advances_not_only_the_first_of_its_list() {
        let (scheduler, store) = scheduler().await;
        deploy(&store, None, &["*/5 * * * *", "*/10 * * * *"]).await;
        scheduler.refresh().await;

        let now = Utc::now();
        let was_due = now - chrono::Duration::minutes(1);
        make_due(&scheduler, was_due);

        scheduler
            .fire_due(now, &crate::shutdown::Shutdown::new())
            .await;

        for task in scheduler.tasks_for("demo", "production") {
            assert_eq!(
                task.last_run,
                Some(was_due),
                "{} was not recorded",
                task.cron_expr
            );
            assert!(
                task.next_run.is_some_and(|next| next > now),
                "{} stayed due and would fire again on the next tick",
                task.cron_expr
            );
        }

        // A second tick with nothing newly due must fire nothing at all.
        scheduler
            .fire_due(now, &crate::shutdown::Shutdown::new())
            .await;
        for task in scheduler.tasks_for("demo", "production") {
            assert_eq!(task.last_run, Some(was_due));
        }
    }

    /// The status tonic actually produces when the trigger deadline expires.
    ///
    /// `Endpoint::timeout` elapsing becomes `Cancelled`, not
    /// `DeadlineExceeded`; a fix that only looked for the latter would leave
    /// the ordinary case - a cron handler that outran the 60 second deadline -
    /// reading exactly as it did before.
    fn timed_out() -> NodeError {
        NodeError::Rpc {
            code: tonic::Code::Cancelled,
            message: "Timeout expired".to_string(),
        }
    }

    /// A trigger that came back without an answer must not be handed on.
    ///
    /// The walk used to read every failed call as "nothing happened on this
    /// node" and carry on down the fleet. A handler still running on node-a
    /// when the deadline expired was therefore started again on node-b and
    /// again on node-c: one due occurrence, three charges. Nothing here can
    /// tell a node that never got the request from a node that got it and is
    /// still working, so the only safe reading of "no answer" is the one that
    /// does not start a second copy.
    #[tokio::test]
    async fn a_trigger_that_came_back_without_an_answer_stops_the_walk() {
        let (scheduler, store) = scheduler_with_nodes(&["node-a", "node-b", "node-c"]).await;
        deploy(&store, None, &["*/5 * * * *"]).await;
        scheduler.refresh().await;

        let now = Utc::now();
        let asked = std::sync::Mutex::new(Vec::<String>::new());
        let outcome = scheduler
            .fire_on_fleet(&due_now(now), now, |node| {
                asked.lock().unwrap().push(node.clone());
                async move { Err(timed_out()) }
            })
            .await;

        assert_eq!(
            asked.into_inner().unwrap(),
            ["node-a"],
            "the handler may still be running on node-a; every further node starts a second copy"
        );
        assert!(
            matches!(outcome, RunOutcome::Unknown { ref node, .. } if node == "node-a"),
            "the occurrence has to be recorded as unknown on node-a, not as one nobody ran: {outcome:?}"
        );
    }

    /// The other half of the same distinction, and the half that must not
    /// regress: a connection that was never established means the node never
    /// saw the request, so the fleet walk carries on and the occurrence still
    /// runs.
    #[tokio::test]
    async fn a_node_that_was_never_reached_still_hands_the_occurrence_on() {
        let (scheduler, store) = scheduler_with_nodes(&["node-a", "node-b"]).await;
        deploy(&store, None, &["*/5 * * * *"]).await;
        scheduler.refresh().await;

        let now = Utc::now();
        let asked = std::sync::Mutex::new(Vec::<String>::new());
        let outcome = scheduler
            .fire_on_fleet(&due_now(now), now, |node| {
                asked.lock().unwrap().push(node.clone());
                let unreachable = node == "node-a";
                async move {
                    if unreachable {
                        Err(NodeError::Transport(
                            "connect http://10.0.0.7:8086: connection refused".to_string(),
                        ))
                    } else {
                        Ok(answer(true, ""))
                    }
                }
            })
            .await;

        assert_eq!(asked.into_inner().unwrap(), ["node-a", "node-b"]);
        assert_eq!(
            outcome,
            RunOutcome::Ran {
                node: "node-b".to_string()
            }
        );
    }

    /// Which gRPC statuses mean "the node decided not to run this" and which
    /// mean "nobody knows".
    ///
    /// The list runs the safe way round: a status that is not one of the
    /// node's own pre-execution refusals counts as unknown, so a code this
    /// control plane has never seen stops the walk rather than replaying the
    /// occurrence somewhere else.
    #[test]
    fn only_a_refusal_the_node_made_before_running_anything_hands_the_occurrence_on() {
        for code in [
            tonic::Code::Unauthenticated,
            tonic::Code::PermissionDenied,
            tonic::Code::Unimplemented,
            tonic::Code::InvalidArgument,
            tonic::Code::NotFound,
            tonic::Code::FailedPrecondition,
        ] {
            let answer = read_answer(Err(NodeError::Rpc {
                code,
                message: "refused".to_string(),
            }));
            assert!(
                matches!(answer, NodeAnswer::NotRunnableHere(_)),
                "{code} is the node refusing before it ran anything: {answer:?}"
            );
        }

        for code in [
            tonic::Code::Cancelled,
            tonic::Code::DeadlineExceeded,
            tonic::Code::Unavailable,
            tonic::Code::Internal,
            tonic::Code::Unknown,
            tonic::Code::Aborted,
        ] {
            let answer = read_answer(Err(NodeError::Rpc {
                code,
                message: "no answer".to_string(),
            }));
            assert!(
                matches!(answer, NodeAnswer::Unknown(_)),
                "{code} leaves it unknown whether the handler ran: {answer:?}"
            );
        }
    }

    /// The record has to keep "nobody knows" apart from "nobody ran it": one
    /// is an occurrence to go and look at on a named node, the other is an
    /// occurrence that certainly did not happen.
    #[tokio::test]
    async fn an_unknown_outcome_names_the_node_it_was_handed_to() {
        let (scheduler, store) = scheduler_with_nodes(&["node-a"]).await;
        deploy(&store, None, &["*/5 * * * *"]).await;
        scheduler.refresh().await;

        let now = Utc::now();
        let was_due = now - chrono::Duration::minutes(1);
        make_due(&scheduler, was_due);
        let due = due_now(was_due);

        scheduler.record_run(
            &due,
            now,
            RunOutcome::Unknown {
                node: "node-a".to_string(),
                error: "rpc error: Cancelled: Timeout expired".to_string(),
            },
        );

        let task = &scheduler.tasks_for("demo", "production")[0];
        assert_eq!(task.last_node.as_deref(), Some("node-a"));
        assert!(matches!(
            task.last_outcome,
            Some(RunOutcome::Unknown { .. })
        ));
        assert!(
            task.next_run.is_some_and(|next| next > now),
            "an unknown outcome still advances the schedule, or the occurrence re-fires every tick"
        );
    }
}
