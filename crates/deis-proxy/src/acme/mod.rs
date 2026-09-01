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
//!   self-hosted TLS setup dies.
//!
//! Off unless switched on. An operator with their own certificates changes
//! nothing and this module is never constructed.

pub mod account;
pub mod challenge;
pub mod client;
pub mod error;
pub mod http;
pub mod jose;
pub mod names;
pub mod status;
pub mod store;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use openssl::asn1::Asn1Time;
use openssl::x509::X509;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tracing::{debug, error, info, warn};

use crate::config::system::AcmeConfig;
use crate::config::{CertificateConfig, ConfigManager};

pub use challenge::{CHALLENGE_PREFIX, ChallengeResponder};
pub use error::AcmeError;
pub use store::AcmeStore;

use account::AccountKey;
use client::AcmeClient;

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
    /// The files are there and cannot be used, with the reason.
    Unusable(String),
    /// A certificate that parses, with the days of validity it has left. A
    /// negative number means it has already expired.
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
}

/// Everything an enabled `[tls.acme]` adds to the process: the responder the
/// data plane answers challenges from, and the task that keeps certificates
/// current.
pub struct AcmeRuntime {
    pub responder: Arc<ChallengeResponder>,
    pub service: AcmeService,
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
    let service = AcmeService {
        settings,
        config,
        responder: Arc::clone(&responder),
        key_pem,
    };
    Ok(AcmeRuntime { responder, service })
}

/// The renewal task.
pub struct AcmeService {
    settings: AcmeConfig,
    config: Arc<ConfigManager>,
    responder: Arc<ChallengeResponder>,
    /// The account key in the form it is stored in, so a reconnection after
    /// a failure uses the same identity instead of registering another.
    key_pem: Vec<u8>,
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
    /// tables that already decide what this proxy answers to.
    fn desired_names(&self) -> Vec<String> {
        names::desired_names(
            &self.config.routes(),
            &self.config.catalog_routes(),
            self.settings.include_preview_addresses,
            self.settings.max_names,
        )
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
    ) -> Option<Instant> {
        let store = self.store();
        let wanted = self.desired_names();
        if wanted.is_empty() {
            debug!(
                "no name wants an automatic certificate: this instance serves no application \
                 domain and the control plane's routing table names no host that could be \
                 certified over http-01"
            );
            return None;
        }

        // A name that stopped being served keeps its files - somebody may
        // put the route back - but must not keep a retry timer alive.
        state.retain(|name, _| wanted.iter().any(|served| served == name));

        let mut issued = 0usize;
        let mut covered = 0usize;
        let mut failing = 0usize;
        let mut earliest_retry: Option<Instant> = None;
        let note_retry = |instant: Instant, earliest: &mut Option<Instant>| {
            *earliest = Some(earliest.map_or(instant, |current| current.min(instant)));
        };

        for name in &wanted {
            let config = store.certificate_config(name);
            let stored = stored_certificate(&config);
            let days_left = stored.days_left();
            let need = assess(&stored, self.settings.renew_before_days);
            if let Need::Covered { days_left } = need {
                covered += 1;
                debug!(name, days_left, "certificate still current");
                state.remove(name);
                continue;
            }

            // Still inside the backoff from an earlier failure. Skipped
            // rather than retried, and the loop is asked to wake when it is
            // due, so a one-minute backoff is not lost inside a twelve-hour
            // interval.
            if let Some(attempt) = state.get(name)
                && attempt.next_attempt > now
            {
                failing += 1;
                note_retry(attempt.next_attempt, &mut earliest_retry);
                continue;
            }

            match &need {
                Need::Uncertified => info!(
                    name,
                    "no certificate for this name yet; ordering one over http-01"
                ),
                // Loud, and separate from "there is none": a file that is
                // there and cannot be served points at the storage directory
                // rather than at the authority, and ordering a replacement
                // into a directory that is broken will not help.
                Need::Unusable(reason) => warn!(
                    name,
                    reason = %reason,
                    "the stored certificate for this name cannot be served, so a replacement is \
                     being ordered. Check the certificate store: an order spends one of the \
                     certificates the authority allows this domain per week, and it will be spent \
                     again on every pass until the file is usable"
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
                    if let Err(e) =
                        store.save_certificate(name, &certificate.chain_pem, &certificate.key_pem)
                    {
                        // Issued and then lost. The authority has already
                        // counted this certificate against the domain's
                        // weekly allowance and there is nothing on disk to
                        // show for it, so this is worse than a refusal.
                        error!(
                            name,
                            error = %e,
                            "a certificate was issued and could not be stored, so it is gone and \
                             the authority has still counted it against this domain's weekly \
                             limit. Fix the storage directory before the next pass spends another"
                        );
                        let next = self.record_failure(state, name, now);
                        self.report_failure(name, &e, days_left, state, next, now);
                        note_retry(next, &mut earliest_retry);
                        failing += 1;
                        continue;
                    }
                    state.remove(name);
                    issued += 1;
                    // Read back from the file rather than reported from the
                    // response, so the line says what a later pass will
                    // actually find; a certificate that was written and
                    // cannot be read back is caught here rather than on the
                    // next pass.
                    match stored_certificate(&store.certificate_config(name)) {
                        Stored::Valid { days_left } => info!(
                            name,
                            days_valid = days_left,
                            renew_in_days = days_left - self.settings.renew_before_days,
                            "certificate installed; it is served for this name from the next \
                             handshake on"
                        ),
                        other => error!(
                            name,
                            ?other,
                            "a certificate was installed and cannot be read back from the store, \
                             so the next pass will order it again. The authority has already \
                             counted this one against the domain's weekly limit"
                        ),
                    }
                }
                Err(e) => {
                    failing += 1;
                    let next = self.record_failure(state, name, now);
                    self.report_failure(name, &e, days_left, state, next, now);
                    note_retry(next, &mut earliest_retry);
                }
            }
        }

        if issued > 0 {
            // The one line that makes any of this reach traffic: the route
            // table is what the SNI callback reads, and it loads certificates
            // from this store when it is built. Without this a certificate
            // would sit on disk, correct and unused, until the next restart.
            self.config.rebuild_routes();
        }

        let swept = store.sweep_challenges(CHALLENGE_MAX_AGE);
        if swept > 0 {
            debug!(
                swept,
                "challenge answers left behind by orders that never finished were removed"
            );
        }

        info!(
            names = wanted.len(),
            issued, covered, failing, "automatic certificate pass finished"
        );
        earliest_retry
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
        let entry = state.entry(name.to_string()).or_insert(Attempt {
            failures: 0,
            next_attempt: now,
        });
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
                 altogether. Until an attempt succeeds it answers plain HTTP only, and no \
                 redirect sends anyone to the TLS port"
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
pub fn stored_certificate(config: &CertificateConfig) -> Stored {
    let chain = Path::new(&config.cert_file);
    let key = Path::new(&config.key_file);
    if !chain.exists() && !key.exists() {
        return Stored::Missing;
    }

    let pem = match std::fs::read(chain) {
        Ok(pem) => pem,
        Err(e) => {
            return Stored::Unusable(format!("{} cannot be read: {e}", chain.display()));
        }
    };
    if !key.is_file() {
        return Stored::Unusable(format!(
            "{} is there but its private key is not at {}",
            chain.display(),
            key.display()
        ));
    }
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
    let now = match Asn1Time::days_from_now(0) {
        Ok(now) => now,
        Err(e) => return Stored::Unusable(format!("the current time is unreadable: {e}")),
    };
    match now.diff(leaf.not_after()) {
        // `diff` gives whole days plus leftover seconds; the leftover only
        // matters for a certificate in its last day, where rounding towards
        // "renew now" is the safe direction.
        Ok(difference) => Stored::Valid {
            days_left: i64::from(difference.days),
        },
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
            let earliest_retry = self.run_pass(active, &mut state, now).await;
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
