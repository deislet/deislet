//! The two halves of "prove who you are", one for each end of a handshake.
//!
//! Both do the same two things in the same order: let webpki decide whether
//! the certificate chains to our trust root and is currently valid, then read
//! the SPIFFE name out of it and decide whether that name is one we will talk
//! to. Neither ever looks at a host name.
//!
//! # Why the host name is not checked
//!
//! On the web a certificate answers "is this really example.com", and the name
//! being verified is the name that was dialled. Here the certificate answers
//! "is this really the runtime node on 213.19", and the address it was dialled
//! at proves nothing - an address can be taken over, and a machine can be
//! moved. So the client verifier below ignores the `ServerName` rustls hands
//! it and checks the SPIFFE name against the identity the caller said it
//! expected. That is strictly the stronger check: the expected identity comes
//! from the caller's own configuration, not from the network.
//!
//! It also means a caller may dial by IP address with no certificate work at
//! all, which is what a fleet on a private network actually does.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::verify_server_cert_signed_by_trust_anchor;
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error, OtherError, RootCertStore,
    SignatureScheme,
};

use crate::cert::name_in_certificate;
use crate::error::IdentityError;
use crate::spiffe::SpiffeId;

/// Turn one of our refusals into the certificate error rustls sends and logs.
///
/// The reason travels with it. A handshake that fails without saying why is
/// the failure mode this whole crate exists to avoid: two processes that
/// cannot talk, and two logs that both say "bad certificate".
fn refuse(error: IdentityError) -> Error {
    Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(error))))
}

/// Checks the certificate a connecting peer presents.
///
/// Accepts any holder of a certificate that chains to our trust root and
/// carries a usable SPIFFE name. It deliberately does not decide *which*
/// peers may do *what*: that decision needs to know which request is being
/// made, so it belongs at the routes, which read the name off the finished
/// connection. What this guarantees to those routes is that a connection
/// exists at all only if the peer proved a name.
#[derive(Debug)]
pub struct VerifyPeerIsInTrustDomain {
    inner: Arc<dyn ClientCertVerifier>,
    trust_domain: String,
}

impl VerifyPeerIsInTrustDomain {
    /// `trust_domain` is ours: a certificate that chains to our root but names
    /// another trust domain is refused, so a root that has been persuaded to
    /// cross-sign cannot smuggle in a stranger.
    pub fn new(inner: Arc<dyn ClientCertVerifier>, trust_domain: impl Into<String>) -> Self {
        Self {
            inner,
            trust_domain: trust_domain.into(),
        }
    }
}

impl ClientCertVerifier for VerifyPeerIsInTrustDomain {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.inner
            .verify_client_cert(end_entity, intermediates, now)?;

        let name = name_in_certificate(end_entity).map_err(refuse)?;
        if name.trust_domain() != self.trust_domain {
            return Err(refuse(IdentityError::WrongTrustDomain {
                presented: name.to_string(),
                presented_trust_domain: name.trust_domain().to_string(),
                expected_trust_domain: self.trust_domain.clone(),
            }));
        }
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Checks the certificate the far end of an outgoing connection presents,
/// against the one identity the caller said it was dialling.
///
/// This is an equality check on a name the caller configured, so it does not
/// matter what address the connection was made to, what the DNS said, or what
/// the certificate claims about host names.
#[derive(Debug)]
pub struct VerifyPeerIs {
    roots: Arc<RootCertStore>,
    expected: SpiffeId,
    provider: Arc<CryptoProvider>,
}

impl VerifyPeerIs {
    pub fn new(
        roots: Arc<RootCertStore>,
        expected: SpiffeId,
        provider: Arc<CryptoProvider>,
    ) -> Self {
        Self {
            roots,
            expected,
            provider,
        }
    }
}

impl ServerCertVerifier for VerifyPeerIs {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let parsed = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &parsed,
            &self.roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;

        let name = name_in_certificate(end_entity).map_err(refuse)?;
        if name != self.expected {
            return Err(refuse(IdentityError::WrongPeer {
                presented: name.to_string(),
                expected: self.expected.to_string(),
            }));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
