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
use crate::names::{self, ServiceKind};
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

/// Checks the certificate a connecting peer presents against one role.
///
/// [`VerifyPeerIsInTrustDomain`] accepts any machine this fleet signed. Some
/// listeners take traffic from one kind of machine only - the runtime node's
/// data port takes application traffic forwarded by proxies and nothing else
/// - and a name that is in the trust domain but is a node's, or a control
/// plane's, is refused here for the same reason the management plane refuses
/// a node's name: a certificate signed by our own authority is still only the
/// machine its name says it is.
#[derive(Debug)]
pub struct VerifyPeerIsOfKind {
    inner: Arc<dyn ClientCertVerifier>,
    trust_domain: String,
    kind: ServiceKind,
}

impl VerifyPeerIsOfKind {
    /// `trust_domain` is ours and `kind` is the only kind of machine whose
    /// name completes a handshake.
    pub fn new(
        inner: Arc<dyn ClientCertVerifier>,
        trust_domain: impl Into<String>,
        kind: ServiceKind,
    ) -> Self {
        Self {
            inner,
            trust_domain: trust_domain.into(),
            kind,
        }
    }
}

impl ClientCertVerifier for VerifyPeerIsOfKind {
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
        if names::service_kind(&name) != Some(self.kind) {
            return Err(refuse(IdentityError::WrongKind {
                presented: name.to_string(),
                presented_kind: names::service_kind(&name).map_or_else(
                    || "a name this fleet does not issue".to_string(),
                    kind_label,
                ),
                expected_kind: kind_label(self.kind),
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

/// What a [`ServiceKind`] reads as in a refusal.
pub(crate) fn kind_label(kind: ServiceKind) -> String {
    match kind {
        ServiceKind::Control => "a control plane".to_string(),
        ServiceKind::Node => "a runtime node".to_string(),
        ServiceKind::Proxy => "a proxy".to_string(),
        ServiceKind::Store => "a deis-store".to_string(),
        ServiceKind::Queue => "a deis-queue".to_string(),
        ServiceKind::Telemetry => "a deis-telemetry".to_string(),
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

/// Checks the certificate a data service presents to a caller that dialled it
/// by role rather than by name.
///
/// [`VerifyPeerIs`] wants the exact name of the one machine a caller meant to
/// reach, which is what the machine channel has: one control plane, nodes it
/// dials by the name they registered under. The data services are reached
/// through an address the control plane hands out, and what a caller knows
/// about the far end is its role - "a deis-store of this fleet" - not its id.
/// So this accepts any certificate that chains to the trust root, carries one
/// SPIFFE name, and names a machine of `kind`; a certificate our own
/// authority signed for a queue, a node or a proxy is refused, so no other
/// machine of this fleet can stand in for a store and collect the bearer
/// capabilities sent to it.
///
/// `trust_domain` is checked when the caller knows it. A caller holding a
/// machine identity always does (it is the domain of its own name); a person's
/// CLI holding only the fleet's trust root does not, and relies on the root
/// being this fleet's alone.
#[derive(Debug)]
pub struct VerifyServerIsOfKind {
    roots: Arc<RootCertStore>,
    kind: ServiceKind,
    trust_domain: Option<String>,
    provider: Arc<CryptoProvider>,
}

impl VerifyServerIsOfKind {
    pub fn new(
        roots: Arc<RootCertStore>,
        kind: ServiceKind,
        trust_domain: Option<String>,
        provider: Arc<CryptoProvider>,
    ) -> Self {
        Self {
            roots,
            kind,
            trust_domain,
            provider,
        }
    }
}

impl ServerCertVerifier for VerifyServerIsOfKind {
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
        if let Some(expected) = &self.trust_domain
            && name.trust_domain() != expected
        {
            return Err(refuse(IdentityError::WrongTrustDomain {
                presented: name.to_string(),
                presented_trust_domain: name.trust_domain().to_string(),
                expected_trust_domain: expected.clone(),
            }));
        }
        if names::service_kind(&name) != Some(self.kind) {
            return Err(refuse(IdentityError::WrongServiceKind {
                presented: name.to_string(),
                presented_kind: names::service_kind(&name).map_or_else(
                    || "a name this fleet does not issue".to_string(),
                    kind_label,
                ),
                expected_kind: kind_label(self.kind),
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
