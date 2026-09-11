//! Reading the name a certificate carries.
//!
//! This is the one place in the crate that knows what an X.509 certificate
//! looks like from the inside. rustls decides whether a certificate is
//! trustworthy; it deliberately does not offer the subject alternative names
//! back, because for the web the only name that matters is the one being
//! connected to. Here the name *is* the identity, so it has to be read out.
//!
//! # What counts as the name
//!
//! Exactly one URI subject alternative name beginning `spiffe://`. Not the
//! subject common name, which is decoration nobody verifies; not the first of
//! several, because a certificate with two SPIFFE names is a certificate whose
//! holder can be authorised as either one, and which one it gets treated as
//! would then depend on parse order.

use x509_parser::extensions::GeneralName;
use x509_parser::prelude::FromDer;

use crate::error::IdentityError;
use crate::spiffe::SpiffeId;

/// The identity a verified certificate claims.
///
/// The caller is expected to have established that the certificate is
/// trustworthy first: this answers "who does it say it is", not "should it be
/// believed".
pub fn name_in_certificate(der: &[u8]) -> Result<SpiffeId, IdentityError> {
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(der)
        .map_err(|error| IdentityError::UnparsableCertificate(error.to_string()))?;

    let alternative_names = certificate
        .subject_alternative_name()
        .map_err(|error| IdentityError::UnparsableCertificate(error.to_string()))?;

    let mut found: Vec<&str> = Vec::new();
    if let Some(extension) = alternative_names {
        for name in &extension.value.general_names {
            // Matched without regard to case so that a certificate carrying
            // `SPIFFE://...` is refused by name - the parser says which rule
            // it broke - rather than passing for a certificate that carries no
            // SPIFFE name at all.
            if let GeneralName::URI(uri) = name
                && uri.len() >= 7
                && uri[..7].eq_ignore_ascii_case("spiffe:")
            {
                found.push(uri);
            }
        }
    }

    match found.as_slice() {
        [] => Err(IdentityError::NoSpiffeName),
        [only] => Ok(SpiffeId::parse(only)?),
        several => Err(IdentityError::AmbiguousSpiffeName {
            count: several.len(),
            names: several.join(", "),
        }),
    }
}

/// When a certificate starts and stops being valid, in seconds since the
/// epoch.
///
/// Read here rather than left to the TLS stack because rustls only ever
/// answers "is this valid *now*", which is the wrong question for an operator:
/// by the time the answer turns to no, every connection on the machine channel
/// has already started failing at once. Nothing in Deislet issues or renews a
/// certificate, so the only thing it can usefully do is say, early and in
/// absolute terms, when somebody else has to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity {
    pub not_before: i64,
    pub not_after: i64,
}

impl Validity {
    /// How much of this certificate's life has been used at `now`, as a
    /// fraction.
    ///
    /// A fraction rather than a countdown because the useful threshold depends
    /// on the certificate: a week left is early warning on a two-year
    /// certificate and already too late on a two-day one, and Deislet is
    /// handed both. Measuring against the certificate's own lifetime means one
    /// rule fits every issuer, and a freshly issued certificate never trips it.
    ///
    /// Returns `1.0` for a certificate whose validity window is empty or
    /// inverted - there is no life left to spend in one - and clamps to
    /// `[0.0, 1.0]` so a clock that disagrees with the issuer cannot produce a
    /// number no caller is expecting.
    pub fn spent_by(&self, now: i64) -> f64 {
        let lifetime = self.not_after - self.not_before;
        if lifetime <= 0 {
            return 1.0;
        }
        let elapsed = now - self.not_before;
        (elapsed as f64 / lifetime as f64).clamp(0.0, 1.0)
    }

    /// Seconds until this certificate stops being valid, negative once it has.
    pub fn remaining_at(&self, now: i64) -> i64 {
        self.not_after - now
    }
}

/// The validity window of a certificate.
///
/// Like [`name_in_certificate`], this answers what the certificate says rather
/// than whether it should be believed.
pub fn validity_of(der: &[u8]) -> Result<Validity, IdentityError> {
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(der)
        .map_err(|error| IdentityError::UnparsableCertificate(error.to_string()))?;
    let validity = certificate.validity();
    Ok(Validity {
        not_before: validity.not_before.timestamp(),
        not_after: validity.not_after.timestamp(),
    })
}

#[cfg(test)]
mod validity_tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn two_days_from(start: i64) -> Validity {
        Validity {
            not_before: start,
            not_after: start + 2 * DAY,
        }
    }

    /// The rule has to mean the same thing on a two-day demo certificate and
    /// on a two-year one, because this platform is handed both. A threshold in
    /// days would be early notice on one and no notice at all on the other.
    #[test]
    fn the_same_fraction_is_the_same_warning_on_any_lifetime() {
        let short = two_days_from(0);
        let long = Validity {
            not_before: 0,
            not_after: 730 * DAY,
        };
        // Four fifths spent, on both.
        assert!((short.spent_by((2.0 * 0.8 * DAY as f64) as i64) - 0.8).abs() < 1e-9);
        assert!((long.spent_by((730.0 * 0.8 * DAY as f64) as i64) - 0.8).abs() < 1e-9);
    }

    /// The failure this rule exists to avoid twice over: a warning that fires
    /// on every correct startup teaches an operator to skip this crate's
    /// warnings, and the one that matters arrives in a stream they have
    /// learned to ignore.
    #[test]
    fn a_certificate_issued_a_moment_ago_has_nothing_to_report() {
        let issued = two_days_from(1_000);
        assert_eq!(issued.spent_by(1_000), 0.0);
        assert!(issued.spent_by(1_060) < 0.8);
    }

    #[test]
    fn an_expired_certificate_reports_a_negative_remainder_and_a_full_life() {
        let expired = two_days_from(0);
        assert_eq!(expired.remaining_at(3 * DAY), -DAY);
        assert_eq!(expired.spent_by(3 * DAY), 1.0);
    }

    /// A machine whose clock is behind the issuer's would otherwise compute a
    /// negative fraction, and a caller comparing it against thresholds would
    /// silently report nothing. Clamped, the worst a wrong clock can do is
    /// report the certificate as fresh, which is what an unclamped negative
    /// would have meant anyway - but without a number no reader expects.
    #[test]
    fn a_clock_outside_the_window_is_clamped_rather_than_wrapped() {
        let window = two_days_from(1_000_000);
        assert_eq!(window.spent_by(0), 0.0);
        assert_eq!(window.spent_by(i64::MAX / 2), 1.0);
    }

    /// A certificate whose window is empty or inverted has no life left to
    /// spend, and dividing by its lifetime would be a division by zero or a
    /// sign flip. Both are reported as spent, which routes them to the
    /// loudest branch - which is where a certificate nobody can use belongs.
    #[test]
    fn an_empty_or_inverted_window_is_wholly_spent() {
        assert_eq!(
            Validity {
                not_before: 5,
                not_after: 5
            }
            .spent_by(5),
            1.0
        );
        assert_eq!(
            Validity {
                not_before: 10,
                not_after: 1
            }
            .spent_by(5),
            1.0
        );
    }
}
