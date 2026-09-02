//! Automatic certificates: obtaining them, keeping them, and renewing them
//! before they expire.
//!
//! What this module is for is the sentence in the comparison document that
//! said certificates here are files an operator places by hand and renews by
//! hand every sixty days. On a platform whose whole premise is that one
//! person runs it, that is the difference between something to demonstrate
//! and something to put a domain on: the failure mode of a manual renewal is
//! not an error message, it is a site that stops working on a Tuesday.
//!
//! The shape, and the reason for each choice:
//!
//! * **`http-01`, not `dns-01`.** The proxy already owns port 80 for every
//!   name it serves, so it answers the authority's fetch itself - no DNS
//!   provider credentials, no second integration to write per provider, no
//!   third thing to rotate. The price is wildcards, which RFC 8555 issues
//!   only against `dns-01`; a preview zone therefore still wants one
//!   wildcard certificate placed by hand, or [`AcmeConfig`]'s
//!   `include_preview_addresses` turned on to certify each address on its
//!   own. See [`names`].
//! * **The names come from the tables that already route traffic**, not from
//!   a list beside them. See [`names::desired_names`].
//! * **The account key and the certificates live on disk**, split by
//!   authority, and are read before anything is ordered. See [`store`].
//! * **Renewal is a background pass**, not a cron entry somebody has to
//!   install. A failure is logged at a level that matches how much slack is
//!   left, and always says when the next attempt is - a renewal that failed
//!   silently at three in the morning is the single most common way a
//!   self-hosted TLS setup dies. The same numbers are kept in [`health`]
//!   for whatever reports this process's state, because a log nobody ships
//!   is not an alarm.
//! * **What is served and what is ordered are decided by one function.**
//!   [`stored_certificate`] reads a pair and says whether it is usable - the
//!   key matches the certificate, the certificate has not expired - and both
//!   the route table and the renewal pass ask it. A pair the pass would
//!   order again is never one the table is serving, and a pair the table
//!   refuses is one the pass will replace. Before that was so, a key that
//!   stopped matching its certificate was refused by the loader and called
//!   "still current" by the pass for the remaining two months of its life.
//! * **An order is placed only where its result can be stored.** The
//!   authority counts a certificate the moment it signs it, so the pass
//!   checks the store is writable first, and a certificate that was issued
//!   and then could not be written is kept in memory and written on the next
//!   attempt rather than ordered again.
//! * **One renewal writer per store.** Two processes on one storage
//!   directory - two proxies, or the two halves of a zero-downtime upgrade -
//!   would each decide the same name was due. The pass holds a lock in the
//!   store while it runs; a pass that finds it held skips and re-reads the
//!   store later. See [`store`].
//!
//! When a name has no usable certificate - none yet, one that expired, one
//! whose renewal keeps failing - it answers plain HTTP and is not redirected
//! to the TLS port. That is the failure mode the log promises, and the route
//! table is what makes it true.
//!
//! Off unless switched on. An operator with their own certificates changes
//! nothing and this module is never constructed.

pub mod account;
pub mod challenge;
pub mod client;
pub mod error;
pub mod health;
pub mod http;
pub mod jose;
pub mod names;
pub mod status;
pub mod store;

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use openssl::asn1::Asn1Time;
use openssl::pkey::PKey;
use openssl::x509::X509;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tracing::{debug, error, info, warn};

use crate::config::system::AcmeConfig;
use crate::config::{CertificateConfig, ConfigManager};

pub use challenge::{CHALLENGE_PREFIX, ChallengeResponder};
pub use error::AcmeError;
pub use health::{AcmeHealth, HealthSnapshot};
pub use store::AcmeStore;

use account::AccountKey;
use client::{AcmeClient, IssuedCertificate};
use health::PassTally;
use store::read_certificate_pair;

/// How long a published challenge answer may sit in the store before a
/// renewal pass sweeps it. An order that has not finished within an hour has
/// long since been abandoned and retried.
const CHALLENGE_MAX_AGE: Duration = Duration::from_secs(3600);

/// What the certificate store holds for one name.
///
/// "Nothing there" and "something there that cannot be used" are separate
/// answers, and the separation is the point. Folding them together - reading
/// the files with `.ok()?` and calling the result "no certificate" - means a
/// certificate that failed to load for a moment is ordered again, and an
/// order is one of the fifty certificates a week that a registered domain
/// gets. It also means the operator is never told that the file on disk is
/// broken, only that a name keeps being reissued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored {
    /// Neither file is there. Ordinary: it is what every name looks like
    /// before its first certificate.
    Missing,
    /// The files are there and cannot be used, with the reason. A key that
    /// does not match its certificate is this, not `Valid`: the pair cannot
    /// complete a handshake, however many days the certificate has left.
    Unusable(String),
    /// A certificate that parses and whose key matches it, with the whole
    /// days of validity it has left. Negative means it has already expired:
    /// `-1` from the second it ends, so a pair three hours past its end is
    /// never read as one with a day to go.
    Valid { days_left: i64 },
}

impl Stored {
    /// The remaining validity, when there is a certificate to have any.
    /// `None` is what the log lines and the volume of a failure key off.
    pub fn days_left(&self) -> Option<i64> {
        match self {
            Stored::Valid { days_left } => Some(*days_left),
            _ => None,
        }
    }

    /// Whether a handshake with this pair would succeed and be accepted:
    /// the one question the route table asks before loading it.
    pub fn is_servable(&self) -> bool {
        matches!(self, Stored::Valid { days_left } if *days_left >= 0)
    }
}

/// Whether a name needs an order, and how much slack it has if it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// Nothing on disk: this name answers no TLS handshake today.
    Uncertified,
    /// Something is on disk and cannot be served, with the reason. Ordering
    /// is the only way out, but the reason has to be said out loud first:
    /// a storage directory that is broken will break the replacement too.
    Unusable(String),
    /// Inside the renewal window, with this many days of validity left.
    Due { days_left: i64 },
    /// Outside the renewal window; nothing to do.
    Covered { days_left: i64 },
}

/// What the store's answer means for this pass.
///
/// A pure decision so it can be tested at the boundaries that matter -
/// exactly at the window, one day inside, already expired, unreadable -
/// without an authority, a clock, or a certificate.
pub fn assess(stored: &Stored, renew_before_days: i64) -> Need {
    match stored {
        Stored::Missing => Need::Uncertified,
        Stored::Unusable(reason) => Need::Unusable(reason.clone()),
        Stored::Valid { days_left } if *days_left <= renew_before_days => Need::Due {
            days_left: *days_left,
        },
        Stored::Valid { days_left } => Need::Covered {
            days_left: *days_left,
        },
    }
}

/// How loudly a failed attempt should be reported.
///
/// The distinction is the whole point of the log line. A first failure for a
/// name whose certificate is good for another month is a retry; the same
/// failure a month later, with days to go, is an outage being announced in
/// advance, and somebody has to see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loudness {
    Warn,
    Error,
}

pub fn loudness(days_left: Option<i64>, failures: u32, alarm_before_days: i64) -> Loudness {
    match days_left {
        // Serving HTTPS for this name is not working now and has not started
        // working. The first attempt can fail for an ordinary reason - a DNS
        // record pointed at this machine a minute ago - so it is a warning;
        // by the second, somebody needs to look.
        None if failures >= 2 => Loudness::Error,
        None => Loudness::Warn,
        Some(days_left) if days_left <= alarm_before_days => Loudness::Error,
        Some(_) => Loudness::Warn,
    }
}

/// Per-name retry state, kept in the renewal task and nowhere else.
struct Attempt {
    failures: u32,
    /// Earliest instant a new order may be started for this name.
    next_attempt: Instant,
    /// A certificate the authority issued and the store could not take.
    /// Held so the next attempt writes it instead of ordering another: the
    /// authority has already counted this one, and a store that was broken
    /// for a minute must not cost a certificate per retry.
    unsaved: Option<IssuedCertificate>,
}

impl Attempt {
    fn new(now: Instant) -> Self {
        Self {
            failures: 0,
            next_attempt: now,
            unsaved: None,
        }
    }
}

/// Drop the retry state of names the table no longer serves, except for a
/// grace period after their last scheduled attempt.
///
/// Without the grace, a route that left the table and came back between two
/// wakes arrived with no `Attempt` and was ordered at once, however many
/// times it had just failed - a flapping route reset its own backoff. The
/// grace is one maximum backoff past the attempt the name was waiting for;
/// a name gone for longer than that is a name somebody removed.
fn prune_attempts(
    state: &mut HashMap<String, Attempt>,
    wanted: &[String],
    now: Instant,
    grace: Duration,
) {
    state.retain(|name, attempt| {
        wanted.iter().any(|served| served == name) || now < attempt.next_attempt + grace
    });
}

/// Write an issued certificate, or keep it for the next attempt.
///
/// On failure the pair goes into `attempt.unsaved` rather than being
/// dropped: it has been counted against the domain's weekly allowance
/// whether or not it reaches disk, so the next attempt for this name writes
/// it again instead of asking the authority for another.
fn save_or_keep(
    store: &AcmeStore,
    name: &str,
    issued: IssuedCertificate,
    attempt: &mut Attempt,
) -> Result<(), AcmeError> {
    match store.save_certificate(name, &issued.chain_pem, &issued.key_pem) {
        Ok(()) => {
            attempt.unsaved = None;
            Ok(())
        }
        Err(e) => {
            attempt.unsaved = Some(issued);
            Err(e)
        }
    }
}

/// Everything an enabled `[tls.acme]` adds to the process: the responder the
/// data plane answers challenges from, the task that keeps certificates
/// current, and the counters that say how that is going.
pub struct AcmeRuntime {
    pub responder: Arc<ChallengeResponder>,
    pub service: AcmeService,
    /// Updated by every pass; read by whatever reports this process's
    /// state. Nothing in this module reads it back.
    pub health: Arc<AcmeHealth>,
}

/// Build the responder and the renewal task, and settle the account key.
///
/// The key is read - or created - here, in the foreground, so that a storage
/// directory that cannot be written is a startup failure with a message
/// rather than a background task that quietly never issues anything.
pub fn build(settings: AcmeConfig, config: Arc<ConfigManager>) -> Result<AcmeRuntime, AcmeError> {
    let store = AcmeStore::new(Path::new(&settings.storage_dir), &settings.directory_url);
    store.prepare()?;
    let (key, created) = store.load_or_create_account_key()?;
    if created {
        info!(
            path = %store.account_key_path().display(),
            "a new ACME account key was generated; it is the identity this installation is known \
             by and is kept across restarts from here on. Generating one per start would spend \
             the authority's per-address registration limit on restarts"
        );
    } else {
        info!(
            path = %store.account_key_path().display(),
            "the stored ACME account key was loaded, so this start registers nothing new"
        );
    }
    let stored = store.stored_certificates();
    info!(
        certificates = stored.len(),
        directory = %store.root().display(),
        "automatic certificates found on disk; these are served immediately and only reordered \
         once they enter the renewal window"
    );

    // The key is kept as PEM rather than as a live key, because every
    // reconnection after a failed pass needs one and an `AccountKey` is
    // moved into the client that owns the connection.
    let key_pem = key.to_pem()?;
    let responder = Arc::new(ChallengeResponder::new(AcmeStore::new(
        Path::new(&settings.storage_dir),
        &settings.directory_url,
    )));
    let health = Arc::new(AcmeHealth::new());
    let service = AcmeService {
        settings,
        config,
        responder: Arc::clone(&responder),
        key_pem,
        health: Arc::clone(&health),
    };
    Ok(AcmeRuntime {
        responder,
        service,
        health,
    })
}

/// The renewal task.
pub struct AcmeService {
    settings: AcmeConfig,
    config: Arc<ConfigManager>,
    responder: Arc<ChallengeResponder>,
    /// The account key in the form it is stored in, so a reconnection after
    /// a failure uses the same identity instead of registering another.
    key_pem: Vec<u8>,
    health: Arc<AcmeHealth>,
}

impl AcmeService {
    /// The store this task reads and writes.
    fn store(&self) -> AcmeStore {
        AcmeStore::new(
            Path::new(&self.settings.storage_dir),
            &self.settings.directory_url,
        )
    }

    /// The names this pass should hold certificates for, taken from the two
    /// tables that already decide what this proxy answers to, and the names
    /// that wanted one and did not fit under the ceiling.
    fn select_names(&self) -> names::NameSelection {
        names::select_names(
            &self.config.routes(),
            &self.config.catalog_routes(),
            self.settings.include_preview_addresses,
            self.settings.max_names,
        )
    }

    /// Say, on every pass, which names are not being covered and why.
    ///
    /// Every pass rather than once, because the consequence is not
    /// immediate: a name pushed out of the ceiling that already holds a
    /// certificate keeps being served until that certificate expires, weeks
    /// after a one-time warning scrolled away, and on that day it stops
    /// answering HTTPS with nothing in the recent log to say why.
    fn report_uncovered(&self, dropped: &[names::DroppedName]) {
        if dropped.is_empty() {
            return;
        }
        for name in dropped {
            warn!(
                name = %name.name,
                owner = %name.owner,
                max_names = self.settings.max_names,
                "this name wants an automatic certificate and does not fit under \
                 tls.acme.max_names, so nothing is obtained or renewed for it. It answers plain \
                 HTTP without a redirect once whatever certificate it holds has expired"
            );
        }
        warn!(
            uncovered = dropped.len(),
            max_names = self.settings.max_names,
            "more names want automatic certificates than tls.acme.max_names allows. The ceiling \
             is shared: this instance's own configured names are taken first, and what is left \
             goes round the applications one name each, so no single application can push the \
             others out. The ceiling is there because a certificate authority counts new \
             certificates per registered domain per week and refuses every name under the domain \
             once that is spent, production included. Raise max_names deliberately, or give the \
             extra names one certificate placed by hand"
        );
    }

    /// Make the route table agree with the store.
    ///
    /// The table loads automatic certificates when it is built, and it is
    /// built on a configuration change or when this pass issued something.
    /// Neither happens when a certificate expires while loaded, when another
    /// process on the same store wrote one, or when somebody replaced a pair
    /// by hand - so this pass compares what the store can serve with what
    /// the table is serving and rebuilds when the two differ. That is what
    /// takes an expired certificate out of the handshake, and its name out
    /// of the redirect, on the pass after it expired.
    fn reconcile_routes(&self, store: &AcmeStore, issued: usize) {
        let routes = self.config.routes();
        let servable: BTreeSet<String> = store
            .stored_certificates()
            .into_iter()
            .filter(|(name, config)| {
                !routes.has_configured_certificate_for(name)
                    && stored_certificate(config).is_servable()
            })
            .map(|(name, _)| name)
            .collect();
        let serving = routes.automatic_certificate_names();
        if issued == 0 && servable == *serving {
            return;
        }
        if issued == 0 {
            let gone: Vec<&String> = serving.difference(&servable).collect();
            let new: Vec<&String> = servable.difference(serving).collect();
            info!(
                no_longer_servable = ?gone,
                newly_servable = ?new,
                "the certificate store and the route table disagree about which automatic \
                 certificates can be served; rebuilding the table so the handshake and the HTTPS \
                 redirect follow the store. A certificate that expired while loaded stops being \
                 presented here"
            );
        }
        // The one line that makes any of this reach traffic: the route
        // table is what the SNI callback reads, and it loads certificates
        // from this store when it is built. Without this a certificate
        // would sit on disk, correct and unused, until the next restart.
        self.config.rebuild_routes();
    }

    /// Reach the authority and make sure the account key is registered.
    async fn connect(&self) -> Result<AcmeClient, AcmeError> {
        let key = AccountKey::from_pem(&self.key_pem)?;
        let store = self.store();
        let mut client = AcmeClient::connect(
            &self.settings.directory_url,
            key,
            store.load_account_url(),
            Duration::from_secs(self.settings.request_timeout_secs),
            Duration::from_secs(self.settings.order_timeout_secs),
        )
        .await?;
        client
            .ensure_account(
                &self.settings.contact_email,
                self.settings.terms_of_service_agreed,
            )
            .await?;
        if let Some(url) = client.account_url()
            && store.load_account_url().as_deref() != Some(url)
            && let Err(e) = store.save_account_url(url)
        {
            warn!(
                error = %e,
                "the ACME account URL could not be written to the store; the account itself is \
                 unaffected, but the next start has to look it up again"
            );
        }
        Ok(client)
    }

    /// One renewal pass over every wanted name.
    ///
    /// Returns the earliest instant some name wants another attempt, so the
    /// loop can wake for a backoff instead of sleeping a whole check
    /// interval through it.
    async fn run_pass(
        &self,
        client: &mut AcmeClient,
        state: &mut HashMap<String, Attempt>,
        now: Instant,
        shutdown: &ShutdownWatch,
    ) -> Option<Instant> {
        let store = self.store();
        let retry_soon = || {
            now + backoff_for(
                1,
                self.settings.retry_backoff_secs,
                self.settings.max_retry_backoff_secs,
            )
        };

        // Held for the whole pass. A second process on this store - another
        // proxy, or the other half of an upgrade - skips its pass while this
        // one runs and finds whatever was ordered on disk afterwards, instead
        // of ordering the same names again.
        let _renewal_lock = match store.try_renewal_lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                self.health.record_skipped_locked();
                info!(
                    store = %store.root().display(),
                    retry_in_secs = retry_soon().saturating_duration_since(now).as_secs(),
                    "another process is running a renewal pass on this certificate store, so \
                     this pass is skipped; the store is re-read once it is free. Two proxies \
                     sharing one storage directory is supported as one writer at a time"
                );
                return Some(retry_soon());
            }
            Err(e) => {
                error!(
                    store = %store.root().display(),
                    error = %e,
                    "the certificate store's renewal lock cannot be taken, so nothing is ordered: \
                     a store that cannot hold a lock file cannot hold a certificate either"
                );
                return Some(retry_soon());
            }
        };

        let selection = self.select_names();
        let wanted = selection.wanted;
        self.report_uncovered(&selection.dropped);
        if wanted.is_empty() && selection.dropped.is_empty() {
            debug!(
                "no name wants an automatic certificate: this instance serves no application \
                 domain and the control plane's routing table names no host that could be \
                 certified over http-01"
            );
        }

        // A name that stopped being served keeps its files - somebody may
        // put the route back - and keeps its backoff for a while too, so a
        // route that flaps cannot reset its own retry timer.
        prune_attempts(
            state,
            &wanted,
            now,
            Duration::from_secs(self.settings.max_retry_backoff_secs),
        );

        let mut tally = PassTally {
            wanted: wanted.len(),
            uncovered: selection.dropped.len(),
            ..Default::default()
        };
        let mut earliest_retry: Option<Instant> = None;
        let note_retry = |instant: Instant, earliest: &mut Option<Instant>| {
            *earliest = Some(earliest.map_or(instant, |current| current.min(instant)));
        };
        let mut days_left_min: Option<i64> = None;
        let mut stopped_early = false;

        for name in &wanted {
            // Checked between names, not only between passes: the outgoing
            // half of an upgrade used to keep ordering through SIGQUIT while
            // its replacement started its own first pass. An order already in
            // flight finishes and is written; the next one is not started.
            if *shutdown.borrow() {
                stopped_early = true;
                break;
            }

            let config = store.certificate_config(name);
            let stored = stored_certificate(&config);
            let days_left = stored.days_left();
            let need = assess(&stored, self.settings.renew_before_days);
            if let Need::Covered { days_left } = need {
                tally.covered += 1;
                days_left_min = Some(days_left_min.map_or(days_left, |d| d.min(days_left)));
                debug!(name, days_left, "certificate still current");
                state.remove(name);
                continue;
            }
            if let Some(days) = days_left {
                days_left_min = Some(days_left_min.map_or(days, |d| d.min(days)));
            }

            // Still inside the backoff from an earlier failure. Skipped
            // rather than retried, and the loop is asked to wake when it is
            // due, so a one-minute backoff is not lost inside a twelve-hour
            // interval.
            if let Some(attempt) = state.get(name)
                && attempt.next_attempt > now
            {
                tally.failing += 1;
                if attempt.unsaved.is_some() {
                    tally.unsaved += 1;
                }
                note_retry(attempt.next_attempt, &mut earliest_retry);
                continue;
            }

            // The store has to be able to take the result before the
            // authority is asked for it: an order is counted the moment it
            // is signed, and one spent per retry against a broken directory
            // is the week's allowance gone in an afternoon.
            if let Err(e) = store.probe_writable(name) {
                error!(
                    name,
                    error = %e,
                    "the certificate store cannot be written, so no certificate is ordered for \
                     this name until it can be: an order the store could not keep would still \
                     count against the authority's weekly limit for this domain"
                );
                let next = self.record_failure(state, name, now);
                self.report_failure(name, &e, days_left, state, next, now);
                note_retry(next, &mut earliest_retry);
                tally.failing += 1;
                tally.failed += 1;
                if state.get(name).is_some_and(|a| a.unsaved.is_some()) {
                    tally.unsaved += 1;
                }
                continue;
            }

            // A certificate issued on an earlier attempt that the store
            // could not take then. Written now, and nothing is ordered.
            if let Some(unsaved) = state.get_mut(name).and_then(|a| a.unsaved.take()) {
                let attempt = state
                    .get_mut(name)
                    .expect("the attempt the unsaved certificate was taken from");
                match save_or_keep(&store, name, unsaved, attempt) {
                    Ok(()) => {
                        info!(
                            name,
                            "a certificate issued on an earlier attempt has now been stored; no \
                             new order was placed for it"
                        );
                        state.remove(name);
                        tally.issued += 1;
                        self.report_installed(&store, name);
                    }
                    Err(e) => {
                        tally.saves_failed += 1;
                        tally.unsaved += 1;
                        tally.failing += 1;
                        let next = self.record_failure(state, name, now);
                        self.report_failure(name, &e, days_left, state, next, now);
                        note_retry(next, &mut earliest_retry);
                    }
                }
                continue;
            }

            match &need {
                Need::Uncertified => info!(
                    name,
                    "no certificate for this name yet; ordering one over http-01"
                ),
                // Loud, and separate from "there is none": a pair that is
                // there and cannot be served is either a broken file or a
                // key that stopped matching its certificate, and either way
                // this name has had no HTTPS since it happened. An error,
                // once per order placed for it.
                Need::Unusable(reason) => error!(
                    name,
                    reason = %reason,
                    "the stored certificate for this name cannot be served and has not been, so \
                     a replacement is being ordered. An order spends one of the certificates the \
                     authority allows this domain per week; if this repeats, the store is being \
                     written to by something other than this process"
                ),
                Need::Due { days_left } => info!(
                    name,
                    days_left = *days_left,
                    renew_before_days = self.settings.renew_before_days,
                    "certificate has entered the renewal window; ordering a new one"
                ),
                Need::Covered { .. } => unreachable!("answered above"),
            }

            match client.issue(name, &self.responder).await {
                Ok(certificate) => {
                    let attempt = state
                        .entry(name.to_string())
                        .or_insert_with(|| Attempt::new(now));
                    if let Err(e) = save_or_keep(&store, name, certificate, attempt) {
                        // Issued and not on disk. The authority has counted
                        // it, so it is kept in memory and written on the
                        // next attempt rather than ordered again.
                        error!(
                            name,
                            error = %e,
                            "a certificate was issued and could not be stored. It is kept in \
                             memory and written again on the next attempt without a new order; \
                             it is lost if this process stops first. Fix the storage directory"
                        );
                        tally.saves_failed += 1;
                        tally.unsaved += 1;
                        tally.failing += 1;
                        let next = self.record_failure(state, name, now);
                        self.report_failure(name, &e, days_left, state, next, now);
                        note_retry(next, &mut earliest_retry);
                        continue;
                    }
                    state.remove(name);
                    tally.issued += 1;
                    self.report_installed(&store, name);
                }
                Err(e) => {
                    tally.failing += 1;
                    tally.failed += 1;
                    let next = self.record_failure(state, name, now);
                    self.report_failure(name, &e, days_left, state, next, now);
                    note_retry(next, &mut earliest_retry);
                }
            }
        }

        self.reconcile_routes(&store, tally.issued);

        let swept = store.sweep_challenges(CHALLENGE_MAX_AGE);
        if swept > 0 {
            debug!(
                swept,
                "challenge answers left behind by orders that never finished were removed"
            );
        }

        tally.days_left_min = days_left_min;
        self.health.record_pass(tally);
        if stopped_early {
            info!(
                names = wanted.len(),
                issued = tally.issued,
                covered = tally.covered,
                "automatic certificate pass stopped early because the server is shutting down; \
                 the replacement process picks the remaining names up"
            );
            return None;
        }
        info!(
            names = wanted.len(),
            issued = tally.issued,
            covered = tally.covered,
            failing = tally.failing,
            uncovered = tally.uncovered,
            unsaved = tally.unsaved,
            "automatic certificate pass finished"
        );
        earliest_retry
    }

    /// Say that a certificate reached the store, reading it back from the
    /// file rather than reporting the response, so the line says what a
    /// later pass will actually find; a certificate that was written and
    /// cannot be read back is caught here rather than on the next pass.
    fn report_installed(&self, store: &AcmeStore, name: &str) {
        match stored_certificate(&store.certificate_config(name)) {
            Stored::Valid { days_left } if days_left >= 0 => info!(
                name,
                days_valid = days_left,
                renew_in_days = days_left - self.settings.renew_before_days,
                "certificate installed; it is served for this name from the next handshake on"
            ),
            other => error!(
                name,
                ?other,
                "a certificate was installed and cannot be read back from the store as a usable \
                 pair, so the next pass will order it again. The authority has already counted \
                 this one against the domain's weekly limit"
            ),
        }
    }

    /// Record one more failure for a name and return when it may be tried
    /// again.
    ///
    /// `now` is the instant the pass started rather than this moment, so the
    /// "next attempt in N seconds" the operator reads is measured from the
    /// same clock reading the pass used to decide what was due. A pass over
    /// twenty names takes long enough for the two to differ visibly.
    fn record_failure(
        &self,
        state: &mut HashMap<String, Attempt>,
        name: &str,
        now: Instant,
    ) -> Instant {
        let entry = state
            .entry(name.to_string())
            .or_insert_with(|| Attempt::new(now));
        entry.failures = entry.failures.saturating_add(1);
        entry.next_attempt = now
            + backoff_for(
                entry.failures,
                self.settings.retry_backoff_secs,
                self.settings.max_retry_backoff_secs,
            );
        entry.next_attempt
    }

    /// Say that an attempt failed, at the volume the remaining slack
    /// deserves, and say when the next one is.
    ///
    /// The level is the message. A failure with a month of validity left is
    /// a retry; the same failure with a week left is an outage being
    /// announced in advance, and it has to be visible among everything else
    /// this process logs.
    fn report_failure(
        &self,
        name: &str,
        error: &AcmeError,
        days_left: Option<i64>,
        state: &HashMap<String, Attempt>,
        next_attempt: Instant,
        now: Instant,
    ) {
        let failures = state.get(name).map(|attempt| attempt.failures).unwrap_or(1);
        let retry_in_secs = next_attempt.saturating_duration_since(now).as_secs();
        match loudness(days_left, failures, self.settings.alarm_before_days) {
            Loudness::Error => error!(
                name,
                failures,
                days_left = days_left.unwrap_or_default(),
                has_certificate = days_left.is_some(),
                retry_in_secs,
                rate_limited = error.is_rate_limit(),
                error = %error,
                "obtaining a certificate keeps failing and this name is about to lose HTTPS \
                 altogether. Until an attempt succeeds it serves whatever valid certificate it \
                 still holds; once that has expired, or if it has none, it answers plain HTTP \
                 only and no redirect sends anyone to the TLS port"
            ),
            Loudness::Warn => warn!(
                name,
                failures,
                days_left = days_left.unwrap_or_default(),
                has_certificate = days_left.is_some(),
                retry_in_secs,
                rate_limited = error.is_rate_limit(),
                error = %error,
                "obtaining a certificate failed and will be retried; there is still slack before \
                 this name would stop serving HTTPS"
            ),
        }
    }
}

/// The wait after `failures` consecutive failures: the base doubled each
/// time, up to the ceiling.
///
/// Doubling matters more than the numbers. The usual reason an order fails
/// is that the name does not point at this machine yet, and that can take a
/// day to fix; retrying every minute for a day is a thousand refusals in the
/// authority's logs and in ours.
pub fn backoff_for(failures: u32, base_secs: u64, max_secs: u64) -> Duration {
    let shift = failures.saturating_sub(1).min(32);
    let scaled = base_secs.saturating_mul(1u64 << shift);
    Duration::from_secs(scaled.min(max_secs))
}

/// What the store holds for one name, read from the files themselves.
///
/// Read rather than remembered, because the files are what the route table
/// loads and what a restart finds; a number held in memory would disagree
/// with them the moment somebody replaced a certificate by hand.
///
/// This is the one judge of a pair. The route table loads a pair only when
/// this says `Valid` with days left, and the renewal pass orders a
/// replacement whenever it says anything else - so the two can never
/// disagree about a pair the way they did when the loader checked that the
/// key matched and this only checked that a key file existed.
pub fn stored_certificate(config: &CertificateConfig) -> Stored {
    let chain = Path::new(&config.cert_file);
    let key = Path::new(&config.key_file);
    if !chain.exists() && !key.exists() {
        return Stored::Missing;
    }
    if !key.is_file() {
        return Stored::Unusable(format!(
            "{} is there but its private key is not at {}",
            chain.display(),
            key.display()
        ));
    }
    if !chain.is_file() {
        return Stored::Unusable(format!(
            "{} is there but its certificate chain is not at {}",
            key.display(),
            chain.display()
        ));
    }

    // Both files under the pair lock, so a writer replacing them in another
    // process is never caught between its two renames.
    let (pem, key_pem) = match read_certificate_pair(config) {
        Ok(pair) => pair,
        Err(e) => {
            return Stored::Unusable(format!(
                "{} or {} cannot be read: {e}",
                chain.display(),
                key.display()
            ));
        }
    };
    let leaf = match X509::stack_from_pem(&pem) {
        Ok(parsed) => match parsed.into_iter().next() {
            Some(leaf) => leaf,
            None => {
                return Stored::Unusable(format!("{} holds no certificate", chain.display()));
            }
        },
        Err(e) => {
            return Stored::Unusable(format!("{} is not a PEM chain: {e}", chain.display()));
        }
    };
    let private_key = match PKey::private_key_from_pem(&key_pem) {
        Ok(key) => key,
        Err(e) => {
            return Stored::Unusable(format!("{} is not a private key: {e}", key.display()));
        }
    };
    match leaf.public_key() {
        Ok(public_key) if public_key.public_eq(&private_key) => {}
        Ok(_) => {
            // The shape a crash between the two renames leaves, and the
            // shape a second writer on the same store produces. The pair
            // cannot complete a handshake, so however many days the
            // certificate has left it is not one this name can use.
            return Stored::Unusable(format!(
                "the private key in {} does not match the certificate in {}",
                key.display(),
                chain.display()
            ));
        }
        Err(e) => {
            return Stored::Unusable(format!(
                "the public key in {} cannot be read: {e}",
                chain.display()
            ));
        }
    }
    let now = match Asn1Time::days_from_now(0) {
        Ok(now) => now,
        Err(e) => return Stored::Unusable(format!("the current time is unreadable: {e}")),
    };
    match now.diff(leaf.not_after()) {
        // `diff` gives whole days plus leftover seconds, both negative once
        // the end has passed. The leftover matters twice: in the last day,
        // where rounding towards "renew now" is the safe direction, and in
        // the first day after the end, where zero days and negative seconds
        // is an expired certificate and must read as one.
        Ok(difference) => {
            let days = i64::from(difference.days);
            let expired = days < 0 || (days == 0 && difference.secs < 0);
            Stored::Valid {
                days_left: if expired { days.min(-1) } else { days },
            }
        }
        Err(e) => Stored::Unusable(format!(
            "the expiry date in {} cannot be read: {e}",
            chain.display()
        )),
    }
}

#[async_trait]
impl BackgroundService for AcmeService {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        info!(
            directory = %self.settings.directory_url,
            storage_dir = %self.settings.storage_dir,
            renew_before_days = self.settings.renew_before_days,
            alarm_before_days = self.settings.alarm_before_days,
            check_interval_secs = self.settings.check_interval_secs,
            max_names = self.settings.max_names,
            include_preview_addresses = self.settings.include_preview_addresses,
            "automatic certificate renewal started"
        );

        let mut client: Option<AcmeClient> = None;
        let mut state: HashMap<String, Attempt> = HashMap::new();
        let check_interval = Duration::from_secs(self.settings.check_interval_secs);
        let mut connect_failures = 0u32;
        // The first pass runs at once: an instance starting with no
        // certificate for a name it already serves should not wait a whole
        // interval before saying so.
        let mut delay = Duration::ZERO;

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    info!("automatic certificate renewal stopping with the server");
                    return;
                }
                // A scheduled wake, not a wait for something to happen. What
                // decides whether anything is ordered is read from the store
                // and from the route table after this returns.
                _ = tokio::time::sleep(delay) => {}
            }

            if client.is_none() {
                match self.connect().await {
                    Ok(connected) => {
                        connect_failures = 0;
                        client = Some(connected);
                    }
                    Err(e) => {
                        connect_failures = connect_failures.saturating_add(1);
                        self.health.record_connect_failure();
                        let backoff = backoff_for(
                            connect_failures,
                            self.settings.retry_backoff_secs,
                            self.settings.max_retry_backoff_secs,
                        );
                        // Error rather than warn whatever the slack is: this
                        // is not one name failing, it is every name failing,
                        // and the certificates already on disk are now on a
                        // countdown with nothing renewing them.
                        error!(
                            directory = %self.settings.directory_url,
                            failures = connect_failures,
                            retry_in_secs = backoff.as_secs(),
                            rate_limited = e.is_rate_limit(),
                            error = %e,
                            "the certificate authority could not be reached or the account could \
                             not be registered, so nothing can be issued or renewed at all. \
                             Certificates already on disk keep serving until they expire"
                        );
                        delay = backoff;
                        continue;
                    }
                }
            }

            let Some(active) = client.as_mut() else {
                // Unreachable: the block above either set it or continued.
                delay = check_interval;
                continue;
            };
            let now = Instant::now();
            let earliest_retry = self.run_pass(active, &mut state, now, &shutdown).await;
            let until_retry = earliest_retry
                .map(|instant| instant.saturating_duration_since(Instant::now()))
                .unwrap_or(check_interval);
            // Never zero: a name whose backoff has already elapsed would
            // otherwise spin this loop against the authority.
            delay = until_retry
                .min(check_interval)
                .max(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(days_left: i64) -> Stored {
        Stored::Valid { days_left }
    }

    #[test]
    fn the_renewal_window_is_judged_at_its_edges() {
        // Exactly at the window renews: the alternative is a certificate
        // that first becomes due the day after the window opened, which
        // silently shortens every operator's configured slack by a day.
        assert_eq!(assess(&Stored::Missing, 30), Need::Uncertified);
        assert_eq!(assess(&valid(31), 30), Need::Covered { days_left: 31 });
        assert_eq!(assess(&valid(30), 30), Need::Due { days_left: 30 });
        assert_eq!(assess(&valid(0), 30), Need::Due { days_left: 0 });
        assert_eq!(assess(&valid(-5), 30), Need::Due { days_left: -5 });
    }

    #[test]
    fn a_certificate_that_cannot_be_read_is_not_reported_as_absent() {
        // The two answers lead to the same order and to two very different
        // log lines. Told apart, an operator learns their storage directory
        // is broken; folded together, they see a name being reissued every
        // pass and no reason for it - while each pass spends one of the
        // authority's weekly certificates.
        let unusable = Stored::Unusable("it is not a PEM chain".to_string());
        assert_eq!(
            assess(&unusable, 30),
            Need::Unusable("it is not a PEM chain".to_string())
        );
        assert_ne!(assess(&unusable, 30), Need::Uncertified);
        assert_eq!(unusable.days_left(), None);
        assert_eq!(valid(12).days_left(), Some(12));
    }

    #[test]
    fn a_failure_gets_louder_as_the_slack_runs_out() {
        // The one property that decides whether an operator sees the outage
        // coming: the same failure has to be a warning with a month to go
        // and an error with a week.
        assert_eq!(loudness(Some(20), 1, 7), Loudness::Warn);
        assert_eq!(loudness(Some(20), 99, 7), Loudness::Warn);
        assert_eq!(loudness(Some(7), 1, 7), Loudness::Error);
        assert_eq!(loudness(Some(-1), 1, 7), Loudness::Error);
        // A name with no certificate at all: once, because a DNS record may
        // have been pointed here a minute ago; twice, and somebody has to
        // look at it.
        assert_eq!(loudness(None, 1, 7), Loudness::Warn);
        assert_eq!(loudness(None, 2, 7), Loudness::Error);
    }

    #[test]
    fn the_backoff_doubles_and_then_stops_doubling() {
        assert_eq!(backoff_for(1, 60, 21600), Duration::from_secs(60));
        assert_eq!(backoff_for(2, 60, 21600), Duration::from_secs(120));
        assert_eq!(backoff_for(3, 60, 21600), Duration::from_secs(240));
        assert_eq!(backoff_for(9, 60, 21600), Duration::from_secs(15360));
        // Capped rather than overflowing: a name that has failed for a week
        // must still be retried, and `1 << 40` seconds is not a retry.
        assert_eq!(backoff_for(10, 60, 21600), Duration::from_secs(21600));
        assert_eq!(backoff_for(500, 60, 21600), Duration::from_secs(21600));
    }

    #[test]
    fn a_missing_certificate_reads_as_no_certificate_rather_than_as_expired() {
        // The two are handled differently - one orders, the other renews -
        // and a missing file that read as "expired" would put a name that
        // has never had a certificate into the error path on its first pass.
        let stored = stored_certificate(&CertificateConfig {
            cert_file: "/nonexistent/deis-proxy-acme-test.pem".to_string(),
            key_file: "/nonexistent/deis-proxy-acme-test.key".to_string(),
        });
        assert_eq!(stored, Stored::Missing);
        assert_eq!(assess(&stored, 30), Need::Uncertified);
    }

    #[test]
    fn a_chain_without_its_key_beside_it_says_which_file_is_missing() {
        // The shape a half-finished write leaves behind, and the one a
        // person leaves behind by copying only the visible file across.
        let dir = tempfile::tempdir().unwrap();
        let chain = dir.path().join("fullchain.pem");
        std::fs::write(&chain, b"-----BEGIN CERTIFICATE-----\n").unwrap();
        let stored = stored_certificate(&CertificateConfig {
            cert_file: chain.to_string_lossy().into_owned(),
            key_file: dir
                .path()
                .join("privkey.pem")
                .to_string_lossy()
                .into_owned(),
        });
        match stored {
            Stored::Unusable(reason) => {
                assert!(reason.contains("privkey.pem"), "{reason}");
                assert!(reason.contains("private key"), "{reason}");
            }
            other => panic!("a chain with no key beside it should be unusable, not {other:?}"),
        }
    }

    /// A pair on disk: a self-signed certificate ending `ending_in_secs`
    /// from now (negative for one already over), and a key. `matching`
    /// decides whether the key written is the certificate's own.
    fn write_pair(dir: &Path, ending_in_secs: i64, matching: bool) -> CertificateConfig {
        use openssl::hash::MessageDigest;
        use openssl::x509::X509NameBuilder;

        let key = PKey::generate_ed25519().unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "shop.example.com").unwrap();
        let name = name.build();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut builder = X509::builder().unwrap();
        builder.set_version(2).unwrap();
        builder.set_subject_name(&name).unwrap();
        builder.set_issuer_name(&name).unwrap();
        builder.set_pubkey(&key).unwrap();
        builder
            .set_not_before(&Asn1Time::from_unix(now + ending_in_secs - 90 * 86_400).unwrap())
            .unwrap();
        builder
            .set_not_after(&Asn1Time::from_unix(now + ending_in_secs).unwrap())
            .unwrap();
        builder.sign(&key, MessageDigest::null()).unwrap();
        let certificate = builder.build();

        let written_key = if matching {
            key
        } else {
            PKey::generate_ed25519().unwrap()
        };
        let chain = dir.join("fullchain.pem");
        let key_file = dir.join("privkey.pem");
        std::fs::write(&chain, certificate.to_pem().unwrap()).unwrap();
        std::fs::write(&key_file, written_key.private_key_to_pem_pkcs8().unwrap()).unwrap();
        CertificateConfig {
            cert_file: chain.to_string_lossy().into_owned(),
            key_file: key_file.to_string_lossy().into_owned(),
        }
    }

    #[test]
    fn a_key_that_does_not_match_its_certificate_is_unusable_not_current() {
        // The pair the route table refuses. Calling it `Valid { 89 }` here
        // made the pass count it as covered on every pass for two months
        // while the name answered no handshake at all; the WARN at load time
        // promised a reorder that this function never asked for.
        let dir = tempfile::tempdir().unwrap();
        let config = write_pair(dir.path(), 89 * 86_400, false);
        let stored = stored_certificate(&config);
        match &stored {
            Stored::Unusable(reason) => {
                assert!(reason.contains("does not match"), "{reason}");
                assert!(reason.contains("privkey.pem"), "{reason}");
            }
            other => panic!("a mismatched pair must be unusable, not {other:?}"),
        }
        assert!(!stored.is_servable());
        assert!(matches!(assess(&stored, 30), Need::Unusable(_)));

        // And the same certificate with its own key is current.
        let config = write_pair(dir.path(), 89 * 86_400, true);
        let stored = stored_certificate(&config);
        assert!(
            matches!(stored, Stored::Valid { days_left } if days_left >= 88),
            "{stored:?}"
        );
        assert!(stored.is_servable());
    }

    #[test]
    fn a_certificate_that_ended_hours_ago_reads_as_expired_not_as_its_last_day() {
        // `diff` answers zero days and negative seconds for the first day
        // after the end. Read as `days_left: 0` that is a certificate in its
        // last day - still served, still redirected onto - when every
        // browser has already refused it.
        let dir = tempfile::tempdir().unwrap();
        let expired = stored_certificate(&write_pair(dir.path(), -3 * 3600, true));
        assert!(
            matches!(expired, Stored::Valid { days_left } if days_left < 0),
            "{expired:?}"
        );
        assert!(!expired.is_servable());
        assert!(matches!(assess(&expired, 30), Need::Due { days_left } if days_left < 0));

        let last_day = stored_certificate(&write_pair(dir.path(), 3 * 3600, true));
        assert_eq!(last_day, Stored::Valid { days_left: 0 });
        assert!(
            last_day.is_servable(),
            "three hours left is still a certificate"
        );
    }

    #[test]
    fn a_route_that_leaves_and_returns_keeps_its_backoff_for_a_while() {
        // Without the grace a name that flapped out of the table between two
        // wakes came back with no `Attempt` and was ordered at once however
        // many times it had just failed.
        let now = Instant::now();
        let mut state = HashMap::new();
        state.insert(
            "flap.example.com".to_string(),
            Attempt {
                failures: 3,
                next_attempt: now + Duration::from_secs(240),
                unsaved: None,
            },
        );
        state.insert(
            "gone.example.com".to_string(),
            Attempt {
                failures: 3,
                next_attempt: now - Duration::from_secs(7200),
                unsaved: None,
            },
        );
        let wanted: Vec<String> = Vec::new();
        prune_attempts(&mut state, &wanted, now, Duration::from_secs(3600));
        assert!(
            state.contains_key("flap.example.com"),
            "a name still inside its backoff keeps it even while it is out of the table"
        );
        assert!(
            !state.contains_key("gone.example.com"),
            "a name whose last attempt is more than a grace period ago is forgotten"
        );
        assert_eq!(state["flap.example.com"].failures, 3);
    }

    #[test]
    fn a_certificate_the_store_could_not_take_is_kept_and_written_later_without_an_order() {
        // The authority has counted the certificate whether or not it
        // reached disk. Dropping it and reordering on the next pass was one
        // certificate per retry against a broken directory - the week's
        // allowance for the domain inside fifteen minutes with the
        // production backoff.
        let temp = tempfile::tempdir().unwrap();
        let store = AcmeStore::new(temp.path(), "https://ca.test/dir");
        store.prepare().unwrap();
        let name = "shop.example.com";
        // A directory where the key file goes: the rename cannot replace it.
        std::fs::create_dir_all(store.certificate_dir(name).join("privkey.pem")).unwrap();
        assert!(store.probe_writable(name).is_err());

        let issued = IssuedCertificate {
            chain_pem: b"chain".to_vec(),
            key_pem: b"key".to_vec(),
        };
        let mut attempt = Attempt::new(Instant::now());
        let error = save_or_keep(&store, name, issued, &mut attempt).unwrap_err();
        assert!(error.to_string().contains("privkey.pem"), "{error}");
        let kept = attempt
            .unsaved
            .as_ref()
            .expect("the issued certificate is kept for the next attempt");
        assert_eq!(kept.chain_pem, b"chain");

        // The operator fixes the directory; the next attempt writes what was
        // kept and the authority is not asked for anything.
        std::fs::remove_dir(store.certificate_dir(name).join("privkey.pem")).unwrap();
        let kept = attempt.unsaved.take().unwrap();
        save_or_keep(&store, name, kept, &mut attempt).unwrap();
        assert!(attempt.unsaved.is_none());
        assert_eq!(
            std::fs::read(store.certificate_config(name).cert_file).unwrap(),
            b"chain"
        );
    }

    #[test]
    fn a_chain_that_is_not_pem_is_unusable_rather_than_absent() {
        let dir = tempfile::tempdir().unwrap();
        let chain = dir.path().join("fullchain.pem");
        let key = dir.path().join("privkey.pem");
        std::fs::write(&chain, b"this is not a certificate").unwrap();
        std::fs::write(&key, b"nor is this a key").unwrap();
        let stored = stored_certificate(&CertificateConfig {
            cert_file: chain.to_string_lossy().into_owned(),
            key_file: key.to_string_lossy().into_owned(),
        });
        assert!(
            matches!(stored, Stored::Unusable(_)),
            "expected an unusable certificate, got {stored:?}"
        );
    }
}
