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
