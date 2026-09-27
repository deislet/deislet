//! What can go wrong, said plainly enough to act on.
//!
//! Two audiences read these. An operator sees them at startup, when the three
//! files are wrong and nothing has served a request yet; and in a log line
//! during a rotation, when the files changed into something unusable and the
//! previous material is still holding the fleet up. Both need to know which
//! file, and what about it.

use std::path::PathBuf;

use crate::spiffe::SpiffeIdError;

/// Why an identity could not be loaded, or a peer could not be named.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("{role} could not be read from {path}: {source}")]
    Unreadable {
        role: FileRole,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} was read as {role} but holds no PEM block this can use")]
    NoPemBlock { role: FileRole, path: PathBuf },

    #[error("{path} was read as {role} and its PEM could not be parsed: {source}")]
    MalformedPem {
        role: FileRole,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "the certificate in {certificate} and the key in {private_key} do not go together, so nothing was replaced: {source}"
    )]
    Mismatched {
        certificate: PathBuf,
        private_key: PathBuf,
        #[source]
        source: rustls::Error,
    },

    #[error("the trust root in {path} was not accepted: {source}")]
    UnusableTrustRoot {
        path: PathBuf,
        #[source]
        source: rustls::Error,
    },

    #[error("a TLS configuration could not be built from this identity: {0}")]
    Tls(#[from] rustls::Error),

    #[error(
        "the certificate in {path} was issued for {lifetime_hours} hour(s), and a machine certificate has to be issued for at least {minimum_hours}: a control-plane outage is ridden out on what a machine already holds, and a certificate that cannot outlast one takes the machine off the channel in the middle of it"
    )]
    LifetimeTooShort {
        path: PathBuf,
        lifetime_hours: i64,
        minimum_hours: i64,
    },

    #[error("a certificate could not be parsed to find the name it carries: {0}")]
    UnparsableCertificate(String),

    #[error(
        "a certificate on this channel carries its holder's name in a URI subject alternative name beginning spiffe://, and this one carries none"
    )]
    NoSpiffeName,

    #[error(
        "a certificate on this channel carries exactly one SPIFFE name, and this one carries {count}: {names}"
    )]
    AmbiguousSpiffeName { count: usize, names: String },

    #[error("the SPIFFE name in a certificate was not usable: {0}")]
    UnusableSpiffeName(#[from] SpiffeIdError),

    #[error(
        "a peer proved the name {presented}, which our trust root signed but which belongs to the trust domain {presented_trust_domain} rather than ours, {expected_trust_domain}"
    )]
    WrongTrustDomain {
        presented: String,
        presented_trust_domain: String,
        expected_trust_domain: String,
    },

    #[error("this connection was made to {expected} and the far end proved it is {presented}")]
    WrongPeer { presented: String, expected: String },

    #[error(
        "a peer proved the name {presented}, which our trust root signed, but it names {presented_kind} rather than the {expected_kind} this listener takes traffic from"
    )]
    WrongKind {
        presented: String,
        presented_kind: String,
        expected_kind: String,
    },

    #[error(
        "this connection carries no peer certificate, so there is no name to read; a listener built from this crate cannot produce such a connection"
    )]
    NoPeerCertificate,
}

/// Which of the three files a message is about.
///
/// A message that says only "could not be read" makes an operator open all
/// three; this makes it one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRole {
    /// This process's own certificate chain.
    Certificate,
    /// This process's own private key.
    PrivateKey,
    /// The trust root every peer's certificate has to chain to.
    TrustRoot,
}

impl std::fmt::Display for FileRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Certificate => "the certificate chain",
            Self::PrivateKey => "the private key",
            Self::TrustRoot => "the trust root",
        })
    }
}
