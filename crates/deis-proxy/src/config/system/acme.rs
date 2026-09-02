//! Automatic certificates: what this instance may ask a certificate
//! authority for, and on whose behalf.
//!
//! Off unless `enabled` says otherwise. An operator who places certificate
//! files by hand and names them in `[tls.default_certificate]` or on an
//! application keeps exactly the behaviour they have; nothing in this table
//! is read until it is switched on.
//!
//! Turning it on costs three things, and they are refused at startup rather
//! than at three in the morning when a certificate expires:
//!
//! * port 80 has to reach this process from the internet, because the
//!   `http-01` challenge is answered on it;
//! * an email address, because that is what the authority writes to when a
//!   certificate is about to expire and nobody has renewed it;
//! * an explicit agreement to the authority's terms of service, because
//!   RFC 8555 requires the account registration to carry one and this
//!   process must not agree to a legal document on an operator's behalf.

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, Validatable};

/// Let's Encrypt's staging environment. The default, and deliberately so:
/// staging issues from an untrusted root, so a misconfiguration costs a
/// browser warning rather than a week of refused orders against production's
/// rate limits, which are counted per registered domain and are not
/// negotiable.
pub const LETS_ENCRYPT_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";
/// Let's Encrypt's production environment. Never a default anywhere; an
/// operator writes this URL out themselves once staging has worked.
pub const LETS_ENCRYPT_PRODUCTION: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// Directory holding the account key and the issued certificates, relative
/// to the directory deis-proxy was started from unless it is absolute.
///
/// Two processes may point at one directory - the two halves of a
/// zero-downtime upgrade do - and the renewal pass holds a lock in it while
/// it runs, so only one of them orders at a time; the other skips its pass
/// and finds the result on disk. The directory has to be writable by this
/// process for a name to be ordered at all: the pass checks that before it
/// asks the authority for anything, because an order is counted against the
/// domain's weekly allowance whether or not its result can be kept.
pub const DEFAULT_ACME_STORAGE_DIR: &str = "acme";
/// Renew once a certificate has less than this many days left. Thirty days
/// on a ninety day certificate leaves room for a month of failed attempts
/// before anything is actually at risk.
pub const DEFAULT_RENEW_BEFORE_DAYS: i64 = 30;
/// Remaining validity under which a failed renewal stops being a warning and
/// becomes an error: at this point the failures have been going on long
/// enough that somebody has to look.
pub const DEFAULT_ALARM_BEFORE_DAYS: i64 = 7;
/// Seconds between renewal passes. Twelve hours: a certificate is renewed a
/// month before it expires, so the pass has sixty chances to notice.
pub const DEFAULT_CHECK_INTERVAL_SECS: u64 = 12 * 60 * 60;
/// Wait after the first failure for one name, doubled on each further one.
pub const DEFAULT_RETRY_BACKOFF_SECS: u64 = 60;
/// Ceiling on that doubling. Six hours keeps a name that cannot be issued -
/// a domain whose DNS does not point here yet is the usual reason - to four
/// attempts a day rather than to a loop against the authority's rate limits.
pub const DEFAULT_MAX_RETRY_BACKOFF_SECS: u64 = 6 * 60 * 60;
/// How many names this instance will ever hold certificates for.
///
/// Let's Encrypt issues at most 50 certificates per registered domain per
/// week. A route table that suddenly grew to hundreds of names - a control
/// plane fault, a preview zone that ran away - must not turn into hundreds
/// of orders, because exhausting that limit stops issuance for *every* name
/// under the domain, production included, for a week.
///
/// A name that does not fit is neither obtained nor renewed. Whatever
/// certificate it already holds is served until it expires, and from then
/// on the name answers plain HTTP with no redirect; every renewal pass names
/// each such host at warn, so the day the ceiling was hit is not the last
/// time the log mentions it.
pub const DEFAULT_MAX_NAMES: usize = 20;
/// Ceiling on one HTTP exchange with the authority.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
/// Ceiling on one order, from `newOrder` to the certificate being downloaded.
/// The authority validates from several vantage points and takes its time
/// about it; past this the order is abandoned and retried with backoff.
pub const DEFAULT_ORDER_TIMEOUT_SECS: u64 = 180;

/// Automatic certificate issue and renewal over ACME (RFC 8555).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AcmeConfig {
    /// Whether to obtain certificates at all. Off by default.
    pub enabled: bool,
    /// The authority's directory resource. Defaults to Let's Encrypt
    /// staging; see [`LETS_ENCRYPT_PRODUCTION`].
    pub directory_url: String,
    /// Address the authority contacts about expiring certificates.
    pub contact_email: String,
    /// Whether the operator agrees to the authority's terms of service.
    /// Registration is refused without it.
    pub terms_of_service_agreed: bool,
    /// Where the account key and the issued certificates are kept.
    pub storage_dir: String,
    /// Renew when fewer than this many days of validity remain.
    pub renew_before_days: i64,
    /// Below this many days of remaining validity, a failed renewal is
    /// logged at error rather than at warn.
    pub alarm_before_days: i64,
    /// Seconds between renewal passes.
    pub check_interval_secs: u64,
    /// Seconds to wait after a name's first failure.
    pub retry_backoff_secs: u64,
    /// Ceiling on the doubling of that wait.
    pub max_retry_backoff_secs: u64,
    /// Ceiling on how many names hold certificates.
    pub max_names: usize,
    /// Seconds allowed for one HTTP exchange with the authority.
    pub request_timeout_secs: u64,
    /// Seconds allowed for one order to reach a downloadable certificate.
    pub order_timeout_secs: u64,
    /// Whether the automatic per-environment addresses synthesised under
    /// `routing.preview_zone` get certificates of their own.
    ///
    /// Off by default, and the reason is the rate limit above rather than
    /// squeamishness: preview environments come and go with branches, and
    /// every one of them would spend one of the fifty certificates a week
    /// that the whole registered domain shares. One wildcard certificate for
    /// the zone covers all of them and costs one - but a wildcard can only be
    /// had through `dns-01`, which this implementation does not speak. Turn
    /// this on for a handful of long-lived environments; place a wildcard by
    /// hand for a zone that churns.
    pub include_preview_addresses: bool,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory_url: LETS_ENCRYPT_STAGING.to_string(),
            contact_email: String::new(),
            terms_of_service_agreed: false,
            storage_dir: DEFAULT_ACME_STORAGE_DIR.to_string(),
            renew_before_days: DEFAULT_RENEW_BEFORE_DAYS,
            alarm_before_days: DEFAULT_ALARM_BEFORE_DAYS,
            check_interval_secs: DEFAULT_CHECK_INTERVAL_SECS,
            retry_backoff_secs: DEFAULT_RETRY_BACKOFF_SECS,
            max_retry_backoff_secs: DEFAULT_MAX_RETRY_BACKOFF_SECS,
            max_names: DEFAULT_MAX_NAMES,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            order_timeout_secs: DEFAULT_ORDER_TIMEOUT_SECS,
            include_preview_addresses: false,
        }
    }
}

impl AcmeConfig {
    /// Whether the configured directory is Let's Encrypt's production
    /// environment, which is worth saying out loud at startup: its rate
    /// limits are low and a mistake against them lasts a week.
    pub fn is_production_authority(&self) -> bool {
        self.directory_url == LETS_ENCRYPT_PRODUCTION
    }

    /// Whether the configured directory is the staging default nobody chose.
    pub fn is_staging_default(&self) -> bool {
        self.directory_url == LETS_ENCRYPT_STAGING
    }
}

impl Validatable for AcmeConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            // Nothing here is read while it is off, and refusing a value an
            // operator left behind in a switched-off table would turn this
            // into a table that cannot be switched off.
            return Ok(());
        }
        let refuse = |message: String| Err(ConfigError::ValidationError { message });

        if !self.directory_url.starts_with("http://") && !self.directory_url.starts_with("https://")
        {
            return refuse(format!(
                "tls.acme.directory_url must be an http:// or https:// URL, not {:?}; it is the \
                 authority's directory resource, for example {LETS_ENCRYPT_STAGING}",
                self.directory_url
            ));
        }
        if !self.contact_email.contains('@') || self.contact_email.len() < 3 {
            return refuse(format!(
                "tls.acme.contact_email must be an email address; {:?} is not one. It is where \
                 the authority writes when a certificate is about to expire and renewal has been \
                 failing, which is the one warning that arrives before an outage",
                self.contact_email
            ));
        }
        if !self.terms_of_service_agreed {
            return refuse(
                "tls.acme.terms_of_service_agreed must be set to true: RFC 8555 registration \
                 carries an agreement to the authority's terms of service, and this process will \
                 not agree to them on your behalf. Read the terms the directory_url publishes, \
                 then say so here"
                    .to_string(),
            );
        }
        if self.renew_before_days < 1 {
            return refuse(
                "tls.acme.renew_before_days must be at least 1; 0 would renew only once the \
                 certificate had already expired"
                    .to_string(),
            );
        }
        if self.alarm_before_days >= self.renew_before_days {
            return refuse(format!(
                "tls.acme.alarm_before_days ({}) must be below renew_before_days ({}); otherwise \
                 the very first failed attempt is logged at error and the level stops \
                 distinguishing a retry that has weeks of slack from one that does not",
                self.alarm_before_days, self.renew_before_days
            ));
        }
        if self.check_interval_secs == 0 {
            return refuse(
                "tls.acme.check_interval_secs must be at least 1; 0 would spin the renewal loop \
                 against the authority with no pause at all"
                    .to_string(),
            );
        }
        if self.retry_backoff_secs == 0 {
            return refuse("tls.acme.retry_backoff_secs must be at least 1".to_string());
        }
        if self.max_retry_backoff_secs < self.retry_backoff_secs {
            return refuse(format!(
                "tls.acme.max_retry_backoff_secs ({}) must not be below retry_backoff_secs ({})",
                self.max_retry_backoff_secs, self.retry_backoff_secs
            ));
        }
        if self.max_names == 0 {
            return refuse(
                "tls.acme.max_names must be at least 1; 0 asks for automatic certificates and \
                 then refuses every name"
                    .to_string(),
            );
        }
        if self.request_timeout_secs == 0 || self.order_timeout_secs == 0 {
            return refuse(
                "tls.acme.request_timeout_secs and order_timeout_secs must be at least 1"
                    .to_string(),
            );
        }
        if self.storage_dir.is_empty() {
            return refuse(
                "tls.acme.storage_dir cannot be empty; it holds the account key, and an account \
                 key that is not kept is a new registration on every start"
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> AcmeConfig {
        AcmeConfig {
            enabled: true,
            contact_email: "ops@example.com".to_string(),
            terms_of_service_agreed: true,
            ..Default::default()
        }
    }

    #[test]
    fn the_default_authority_is_staging_and_never_production() {
        // The single most expensive mistake this feature can make is issuing
        // against production by accident: the rate limits are counted per
        // registered domain, per week, and there is no way to give them back.
        let config = AcmeConfig::default();
        assert_eq!(config.directory_url, LETS_ENCRYPT_STAGING);
        assert!(config.is_staging_default());
        assert!(!config.is_production_authority());
    }

    #[test]
    fn a_switched_off_table_is_never_judged() {
        // An operator who turned it off after a failed experiment keeps
        // whatever they had written; validation must not make the table
        // impossible to switch off.
        let config = AcmeConfig {
            enabled: false,
            contact_email: String::new(),
            directory_url: "not a url".to_string(),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn enabling_without_an_address_or_an_agreement_is_refused() {
        let mut config = enabled();
        config.contact_email = "ops".to_string();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("contact_email"), "{error}");

        let mut config = enabled();
        config.terms_of_service_agreed = false;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("terms_of_service_agreed"), "{error}");
    }

    #[test]
    fn the_alarm_must_sit_inside_the_renewal_window() {
        // Otherwise every first failure is an error and the level carries no
        // information: the operator cannot tell a retry with a month of slack
        // from one that is about to take the site down.
        let mut config = enabled();
        config.renew_before_days = 30;
        config.alarm_before_days = 30;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("alarm_before_days"), "{error}");

        config.alarm_before_days = 7;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_complete_table_validates() {
        assert!(enabled().validate().is_ok());
    }

    #[test]
    fn the_table_round_trips_through_toml() {
        let config: AcmeConfig = toml::from_str(
            r#"
enabled = true
directory_url = "https://acme.example.test/directory"
contact_email = "ops@example.com"
terms_of_service_agreed = true
storage_dir = "/var/lib/deis-proxy/acme"
include_preview_addresses = true
"#,
        )
        .unwrap();
        assert!(config.enabled);
        assert!(config.include_preview_addresses);
        assert_eq!(config.storage_dir, "/var/lib/deis-proxy/acme");
        // Everything unwritten keeps the documented default rather than a
        // zero that would read as "renew never".
        assert_eq!(config.renew_before_days, DEFAULT_RENEW_BEFORE_DAYS);
        assert_eq!(config.max_names, DEFAULT_MAX_NAMES);
        config.validate().unwrap();
    }
}
