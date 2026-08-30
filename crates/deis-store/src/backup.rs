//! Scheduled snapshots: the automation around [`crate::snapshot`].
//!
//! # Why a task inside the service
//!
//! `deis-store snapshot --out DIR` has been correct for a while and nothing
//! ran it. A backup procedure that exists only as a command in a document is
//! not a backup: the data directory is the only copy of every tenant's KV
//! entries, SQL databases and objects, and the first time anyone finds out
//! that nobody wired up cron is the day they need to restore.
//!
//! So the service takes its own snapshots when `snapshot_dir` is configured.
//! An operator who prefers cron, a filesystem snapshot or a backup agent
//! leaves that key unset and loses nothing - the subcommand is unchanged, and
//! this module calls exactly the same [`crate::snapshot::take`] it does.
//!
//! # What the schedule guarantees
//!
//! * **One snapshot at a time.** The tick loop awaits the snapshot it started
//!   before it looks at the clock again, and the ticker skips the ticks that
//!   went by in the meantime rather than firing them in a burst. A snapshot
//!   that takes longer than its own interval therefore delays the next one;
//!   it never stacks two copies of the same data directory on top of each
//!   other, which would double the IO exactly when the store is already slow.
//! * **Prune only after a snapshot that worked.** Retention deletes old
//!   copies to make room for a new one, so a failed snapshot skips the prune
//!   entirely. Deleting last week's good backup because this morning's failed
//!   is the one outcome worse than not pruning.
//! * **Delete only what this schedule wrote.** A directory is a candidate for
//!   pruning only if its name is one this module produces *and* it holds a
//!   finished snapshot manifest. An operator's own directory sitting beside
//!   them is left alone.
//!
//! A failed snapshot is logged at `error`, not `warn`: it means there is no
//! backup of this interval, which is a thing a person has to act on, and the
//! next attempt is a whole interval away.

use crate::error::{StoreError, StoreResult};
use crate::snapshot::{self, MANIFEST_NAME};
use crate::unix_seconds;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

/// Prefix on every directory this module writes. Also what
/// [`SnapshotSchedule::prune`] uses to tell its own output from anything else
/// an operator keeps in the same place.
const SNAPSHOT_PREFIX: &str = "snapshot-";

/// How many snapshots may share one second before the schedule gives up
/// naming them. Only reachable with an interval of a second or two, which is
/// a test rather than a deployment; the bound exists so a name collision
/// cannot become an infinite loop.
const MAX_SNAPSHOTS_PER_SECOND: u32 = 64;

/// What the background task does, resolved from the configuration once at
/// startup.
#[derive(Clone, Debug)]
pub struct SnapshotSchedule {
    /// The data directory being copied.
    pub data_dir: PathBuf,
    /// Where snapshots are written, one subdirectory each.
    pub directory: PathBuf,
    /// How long between snapshots.
    pub interval: Duration,
    /// How many to keep; zero keeps every one of them.
    pub retention: usize,
}

impl SnapshotSchedule {
    /// Take one snapshot now, returning where it landed.
    ///
    /// Blocking, like everything in [`crate::snapshot`]: it is bulk file IO
    /// and SQLite work, so the caller runs it on a blocking thread rather
    /// than holding an async worker for the length of a full copy.
    pub fn take_one(&self) -> StoreResult<PathBuf> {
        let out = self.destination(unix_seconds())?;
        let manifest = snapshot::take(&self.data_dir, &out)?;
        tracing::info!(
            snapshot = %out.display(),
            databases = manifest.databases.len(),
            objects = manifest.objects,
            object_bytes = manifest.object_bytes,
            "scheduled snapshot written"
        );
        Ok(out)
    }

    /// Delete every snapshot older than the newest [`Self::retention`] of
    /// them, returning what was removed.
    ///
    /// Only directories this module wrote are considered: the name has to be
    /// one [`Self::destination`] produces and the directory has to hold a
    /// finished manifest. Anything else in the destination - an operator's
    /// own copy, a snapshot that did not finish - is left where it is.
    pub fn prune(&self) -> StoreResult<Vec<PathBuf>> {
        if self.retention == 0 {
            return Ok(Vec::new());
        }
        let mut snapshots = self.existing()?;
        if snapshots.len() <= self.retention {
            return Ok(Vec::new());
        }
        // Oldest first, so the tail is what is kept.
        snapshots.sort();
        let doomed = snapshots.len() - self.retention;
        let mut removed = Vec::new();
        for (_, _, path) in snapshots.into_iter().take(doomed) {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    tracing::info!(
                        snapshot = %path.display(),
                        retention = self.retention,
                        "deleted a snapshot past the retention count; it can no longer be \
                         restored from"
                    );
                    removed.push(path);
                }
                Err(e) => {
                    // Worth stopping for: the destination is now over its
                    // retention and will stay there, and whatever stopped a
                    // delete will stop the next one too.
                    tracing::error!(
                        snapshot = %path.display(),
                        error = %e,
                        "cannot delete a snapshot past its retention; the snapshot directory \
                         keeps growing and will eventually fill the disk it is on"
                    );
                    return Err(StoreError::Io(e));
                }
            }
        }
        Ok(removed)
    }

    /// A path in the destination that nothing occupies yet.
    ///
    /// Named by the second it was started in, so the directory listing reads
    /// chronologically and a person can tell at a glance which copy is which.
    /// A second that already holds a snapshot gets a sequence suffix rather
    /// than an error: [`snapshot::take`] refuses a destination that is not
    /// empty, and a schedule that failed for a whole interval because two
    /// snapshots landed in one second would be a poor trade.
    fn destination(&self, at: i64) -> StoreResult<PathBuf> {
        for sequence in 0..MAX_SNAPSHOTS_PER_SECOND {
            let path = self.directory.join(snapshot_name(at, sequence));
            if !path.exists() {
                return Ok(path);
            }
        }
        Err(StoreError::InvalidArgument(format!(
            "{} already holds {MAX_SNAPSHOTS_PER_SECOND} snapshots taken in the same second; the \
             snapshot interval is far shorter than a snapshot takes",
            self.directory.display()
        )))
    }

    /// Every finished snapshot in the destination, as `(seconds, sequence,
    /// path)` so the tuple sorts chronologically.
    fn existing(&self) -> StoreResult<Vec<(i64, u32, PathBuf)>> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            // Nothing has been written yet, so there is nothing to prune.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StoreError::Io(e)),
        };

        let mut found = Vec::new();
        let mut unfinished = 0usize;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let Some((at, sequence)) = entry
                .file_name()
                .to_str()
                .and_then(parse_snapshot_name)
                .filter(|_| path.is_dir())
            else {
                continue;
            };
            if !path.join(MANIFEST_NAME).is_file() {
                // A snapshot that was interrupted: it has a name this module
                // produced but no manifest, so it is not restorable and is
                // also not something retention may count or delete.
                unfinished += 1;
                continue;
            }
            found.push((at, sequence, path));
        }

        if unfinished > 0 {
            tracing::warn!(
                directory = %self.directory.display(),
                unfinished,
                manifest = MANIFEST_NAME,
                "the snapshot directory holds snapshots with no manifest, left behind by a copy \
                 that was interrupted. They cannot be restored from, they are not counted \
                 towards the retention and nothing here deletes them; remove them by hand"
            );
        }
        Ok(found)
    }
}

fn snapshot_name(at: i64, sequence: u32) -> String {
    match sequence {
        0 => format!("{SNAPSHOT_PREFIX}{at}"),
        n => format!("{SNAPSHOT_PREFIX}{at}-{n}"),
    }
}

/// The inverse of [`snapshot_name`], and the guard on what may be deleted.
///
/// Anything that is not exactly a name this module produces returns `None`,
/// including a directory an operator named `snapshot-monday`: retention runs
/// unattended and `remove_dir_all` is not an operation to be liberal about.
fn parse_snapshot_name(name: &str) -> Option<(i64, u32)> {
    let rest = name.strip_prefix(SNAPSHOT_PREFIX)?;
    match rest.split_once('-') {
        None => Some((rest.parse().ok()?, 0)),
        Some((at, sequence)) => Some((at.parse().ok()?, sequence.parse().ok()?)),
    }
}

/// Whether `child` sits inside `parent`, as well as can be told without both
/// existing yet.
fn is_inside(child: &Path, parent: &Path) -> bool {
    let resolve = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    resolve(child).starts_with(resolve(parent))
}

/// Run `schedule` until the handle is dropped or aborted.
///
/// The first snapshot is one interval away rather than at startup: a service
/// that is being restarted repeatedly would otherwise spend its time copying
/// its own data directory instead of serving.
pub fn spawn(schedule: SnapshotSchedule) -> JoinHandle<()> {
    tracing::info!(
        directory = %schedule.directory.display(),
        interval_secs = schedule.interval.as_secs(),
        retention = schedule.retention,
        "scheduled snapshots are on"
    );
    if let Err(e) = std::fs::create_dir_all(&schedule.directory) {
        // Reported now rather than at the first tick, so an unwritable
        // destination is visible in the startup log instead of appearing
        // hours later. The task still runs: the usual cause is a mount that
        // is not up yet, and it will start working when it is.
        tracing::error!(
            directory = %schedule.directory.display(),
            error = %e,
            "cannot create the snapshot directory; no snapshot will be written until this is \
             fixed, and the data directory is meanwhile the only copy of every tenant's data"
        );
    }
    if is_inside(&schedule.directory, &schedule.data_dir) {
        tracing::warn!(
            directory = %schedule.directory.display(),
            data_dir = %schedule.data_dir.display(),
            "the snapshot directory is inside the data directory it copies; the snapshots share \
             the fate of the thing they exist to survive, so a lost disk or a deleted data \
             directory takes both. Copy them off this host, or point snapshot_dir elsewhere"
        );
    }
    if schedule.retention == 0 {
        tracing::warn!(
            directory = %schedule.directory.display(),
            "snapshot_retention is 0, so no snapshot is ever deleted; the snapshot directory \
             grows without bound and will eventually fill the disk it is on"
        );
    }

    let interval = schedule.interval;
    spawn_interval(interval, move || {
        let schedule = schedule.clone();
        async move { run_once(&schedule).await }
    })
}

/// The tick loop, separated from what it runs.
///
/// Two properties live here rather than in the snapshot itself, and both are
/// about what happens when a run takes longer than its interval: the loop
/// awaits the run it started before it looks at the clock again, and missed
/// ticks are dropped rather than fired in a burst. Keeping the loop generic
/// is what lets both be checked directly, instead of being inferred from a
/// directory listing after copying a store.
fn spawn_interval<F, Fut>(interval: Duration, mut action: F) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // A run that overran its interval must not be followed by a burst of
        // the ticks it missed - that would queue several full copies of the
        // data directory back to back, at the moment the host is already
        // struggling. Delay instead: one run, then a fresh interval from when
        // it finished.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The first tick fires immediately.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            action().await;
        }
    })
}

/// One scheduled snapshot, plus the prune that follows a successful one.
async fn run_once(schedule: &SnapshotSchedule) {
    let taking = schedule.clone();
    match tokio::task::spawn_blocking(move || taking.take_one()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::error!(
                error = %e,
                directory = %schedule.directory.display(),
                interval_secs = schedule.interval.as_secs(),
                "scheduled snapshot failed; there is no backup of this interval and the next \
                 attempt is a full interval away"
            );
            // No prune after a failure: retention exists to make room for a
            // new snapshot, and there is no new snapshot. Deleting the oldest
            // good copy here would spend a backup to gain nothing.
            return;
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "the scheduled snapshot task did not finish; there is no backup of this interval"
            );
            return;
        }
    }

    let pruning = schedule.clone();
    match tokio::task::spawn_blocking(move || pruning.prune()).await {
        Ok(Ok(removed)) => {
            if !removed.is_empty() {
                tracing::debug!(
                    removed = removed.len(),
                    retention = schedule.retention,
                    "pruned snapshots past the retention count"
                );
            }
        }
        // The snapshot itself succeeded, so this is disk pressure rather than
        // a missing backup: the copy that matters is on disk.
        Ok(Err(e)) => tracing::warn!(
            error = %e,
            directory = %schedule.directory.display(),
            "the snapshot was written but the older ones could not be pruned; the snapshot \
             directory is over its retention and keeps growing"
        ),
        Err(e) => tracing::warn!(
            error = %e,
            "the snapshot prune task did not finish; the snapshot directory is over its retention"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A schedule over a real data directory with something in it, so a
    /// snapshot has work to do rather than copying nothing.
    fn schedule_over(data: &TempDir, out: &TempDir, retention: usize) -> SnapshotSchedule {
        let conn = rusqlite::Connection::open(data.path().join("kv.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kv (
                 tenant     TEXT NOT NULL,
                 key        TEXT NOT NULL,
                 value      BLOB NOT NULL,
                 metadata   TEXT,
                 expires_at INTEGER,
                 PRIMARY KEY (tenant, key)
             )",
        )
        .unwrap();
        drop(conn);
        SnapshotSchedule {
            data_dir: data.path().to_path_buf(),
            directory: out.path().join("snapshots"),
            interval: Duration::from_secs(60),
            retention,
        }
    }

    fn names_in(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory)
            .map(|entries| {
                entries
                    .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// A finished-looking snapshot from `at`, without running a real copy.
    fn plant_snapshot(schedule: &SnapshotSchedule, at: i64, sequence: u32) -> PathBuf {
        let path = schedule.directory.join(snapshot_name(at, sequence));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join(MANIFEST_NAME), b"{}").unwrap();
        path
    }

    #[test]
    fn a_snapshot_lands_where_the_schedule_says() {
        let (data, out) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let schedule = schedule_over(&data, &out, 7);
        let path = schedule.take_one().unwrap();
        assert!(path.starts_with(&schedule.directory));
        assert!(path.join(MANIFEST_NAME).is_file());
        assert!(path.join("kv.sqlite").is_file());
    }

    #[test]
    fn two_snapshots_in_one_second_do_not_collide() {
        // `snapshot::take` refuses a destination that is not empty, so
        // without the sequence suffix a short interval would fail every
        // second snapshot rather than take it.
        let (data, out) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let schedule = schedule_over(&data, &out, 7);
        let first = schedule.destination(1_700_000_000).unwrap();
        std::fs::create_dir_all(&first).unwrap();
        let second = schedule.destination(1_700_000_000).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            parse_snapshot_name(second.file_name().unwrap().to_str().unwrap()),
            Some((1_700_000_000, 1))
        );
    }

    #[test]
    fn retention_keeps_the_newest_and_deletes_the_rest() {
        let (data, out) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let schedule = schedule_over(&data, &out, 2);
        for at in [1_700_000_000, 1_700_000_060, 1_700_000_120, 1_700_000_180] {
            plant_snapshot(&schedule, at, 0);
        }
        let removed = schedule.prune().unwrap();
        assert_eq!(removed.len(), 2);
        assert_eq!(
            names_in(&schedule.directory),
            vec!["snapshot-1700000120", "snapshot-1700000180"]
        );
    }

    #[test]
    fn retention_orders_by_time_and_not_by_name() {
        // Sequence suffixes make the lexical order disagree with the
        // chronological one, which is exactly the case where deleting the
        // wrong directory would go unnoticed.
        let (data, out) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let schedule = schedule_over(&data, &out, 1);
        plant_snapshot(&schedule, 1_700_000_000, 0);
        plant_snapshot(&schedule, 1_700_000_000, 2);
        plant_snapshot(&schedule, 1_700_000_000, 10);
        schedule.prune().unwrap();
        assert_eq!(
            names_in(&schedule.directory),
            vec!["snapshot-1700000000-10"]
        );
    }

    #[test]
    fn a_retention_of_zero_deletes_nothing() {
        let (data, out) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let schedule = schedule_over(&data, &out, 0);
        for at in [1_700_000_000, 1_700_000_060, 1_700_000_120] {
            plant_snapshot(&schedule, at, 0);
        }
        assert!(schedule.prune().unwrap().is_empty());
        assert_eq!(names_in(&schedule.directory).len(), 3);
    }

    #[test]
    fn nothing_this_schedule_did_not_write_is_ever_deleted() {
        // Retention runs unattended, and the destination is a directory an
        // operator may well keep other copies in.
        let (data, out) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let schedule = schedule_over(&data, &out, 1);
        plant_snapshot(&schedule, 1_700_000_000, 0);
        plant_snapshot(&schedule, 1_700_000_060, 0);
        for foreign in [
            "snapshot-monday",
            "before-upgrade",
            "snapshot-",
            "notes.txt",
        ] {
            let path = schedule.directory.join(foreign);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join(MANIFEST_NAME), b"{}").unwrap();
        }
        // An interrupted snapshot of ours: the right name, no manifest.
        let unfinished = schedule.directory.join(snapshot_name(1_600_000_000, 0));
        std::fs::create_dir_all(&unfinished).unwrap();

        schedule.prune().unwrap();
        let left = names_in(&schedule.directory);
        for kept in [
            "snapshot-monday",
            "before-upgrade",
            "snapshot-",
            "notes.txt",
            "snapshot-1600000000",
            "snapshot-1700000060",
        ] {
            assert!(
                left.contains(&kept.to_string()),
                "{kept} must survive: {left:?}"
            );
        }
        assert!(!left.contains(&"snapshot-1700000000".to_string()));
    }

    #[test]
    fn only_names_this_module_writes_are_recognised() {
        assert_eq!(
            parse_snapshot_name("snapshot-1700000000"),
            Some((1_700_000_000, 0))
        );
        assert_eq!(
            parse_snapshot_name("snapshot-1700000000-3"),
            Some((1_700_000_000, 3))
        );
        for foreign in [
            "snapshot-monday",
            "snapshot-",
            "snapshot",
            "snapshot-2026-08-30",
            "backup-1700000000",
            "snapshot-1700000000-x",
            "",
        ] {
            assert_eq!(parse_snapshot_name(foreign), None, "{foreign} is not ours");
        }
    }

    #[tokio::test]
    async fn one_scheduled_run_snapshots_and_then_prunes() {
        // The body the tick loop runs, checked directly: awaiting it is what
        // makes this deterministic, where watching a directory after a timer
        // would be a race.
        let (data, out) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let schedule = schedule_over(&data, &out, 1);
        plant_snapshot(&schedule, 1_600_000_000, 0);

        run_once(&schedule).await;

        let left = names_in(&schedule.directory);
        assert_eq!(left.len(), 1, "retention of 1 should leave one: {left:?}");
        assert!(
            !left.contains(&"snapshot-1600000000".to_string()),
            "the older snapshot should have been pruned: {left:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_snapshot_does_not_prune_the_ones_that_worked() {
        // Retention exists to make room for a new snapshot. With no new
        // snapshot there is nothing to make room for, and deleting the oldest
        // good copy would spend a backup to gain nothing.
        let (data, out) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let mut schedule = schedule_over(&data, &out, 1);
        schedule.data_dir = data.path().join("gone");
        for at in [1_600_000_000, 1_600_000_060, 1_600_000_120] {
            plant_snapshot(&schedule, at, 0);
        }

        run_once(&schedule).await;

        assert_eq!(
            names_in(&schedule.directory).len(),
            3,
            "a failed snapshot must leave every existing backup alone"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_that_overruns_its_interval_does_not_queue_the_ones_it_missed() {
        // Ten intervals pass while one run is still going. A ticker that
        // burst its missed ticks would start ten more runs the moment this
        // one finished - ten full copies of the data directory, back to back,
        // on a host that has just shown it cannot keep up.
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (task_gate, task_runs) = (gate.clone(), runs.clone());
        let started = |counter: &std::sync::Arc<std::sync::atomic::AtomicUsize>| {
            counter.load(std::sync::atomic::Ordering::SeqCst)
        };

        let task = spawn_interval(Duration::from_secs(60), move || {
            let (gate, runs) = (task_gate.clone(), task_runs.clone());
            async move {
                runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Held until the test lets go, standing in for a snapshot
                // that takes longer than its own interval.
                gate.acquire().await.unwrap().forget();
            }
        });

        // Paused time: the clock moves by arithmetic, and the yields hand the
        // task the runtime rather than waiting for it. It has to run once
        // before the clock moves, or it would arm its first tick from the far
        // side of the jump and never see the intervals go by.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(600)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(started(&runs), 1, "one run should be under way");

        gate.add_permits(1);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            started(&runs),
            1,
            "the nine intervals that went by while it ran must not each start a run"
        );

        // The schedule is not stopped, only delayed: the next interval fires.
        gate.add_permits(1);
        tokio::time::advance(Duration::from_secs(60)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(started(&runs), 2, "the schedule should carry on");
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_run_is_one_interval_away_not_at_startup() {
        // A service being restarted in a loop would otherwise spend its life
        // copying its own data directory instead of serving.
        let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task_runs = runs.clone();
        let task = spawn_interval(Duration::from_secs(60), move || {
            let runs = task_runs.clone();
            async move {
                runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0);

        tokio::time::advance(Duration::from_secs(60)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
        task.abort();
    }
}
