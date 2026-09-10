//! The three files, turned into something a listener or a dialler can use.
//!
//! # Why the whole set is built before any of it is adopted
//!
//! A rotation writes three files, and there is no moment at which all three
//! change at once: for a short window on disk the new certificate sits beside
//! the old key. If the process adopted files one at a time it would spend that
//! window presenting a certificate its key does not match, and every handshake
//! in the window would fail for a reason that has nothing to do with either
//! file being wrong.
//!
//! So loading is all-or-nothing. The certificate, the key and the trust root
//! are read, the name is taken out of the certificate, and the server
//! configuration is built - which is where rustls checks that the key really
//! does belong to the certificate. Only a set that survived all of that is
//! handed back, and only then does anything replace what is already in force.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use sha2::{Digest, Sha256};

use crate::IdentityFiles;
use crate::cert::name_in_certificate;
use crate::error::{FileRole, IdentityError};
use crate::spiffe::SpiffeId;
use crate::verify::{VerifyPeerIs, VerifyPeerIsInTrustDomain};

/// One consistent set of identity material, and the configurations built from
/// it. Nothing in here is edited after it is built; a rotation produces a new
/// one and swaps it in whole.
#[derive(Debug)]
pub(crate) struct Material {
    /// The name this process proves to everybody it talks to.
    pub(crate) id: SpiffeId,
    pub(crate) chain: Vec<CertificateDer<'static>>,
    pub(crate) key: PrivateKeyDer<'static>,
    pub(crate) roots: Arc<RootCertStore>,
    pub(crate) server: Arc<ServerConfig>,
    pub(crate) provider: Arc<CryptoProvider>,
    /// What the three files hashed to when this set was built.
    ///
    /// Compared on the next look to answer "is there anything new here" without
    /// keeping a second copy of the private key around to compare against.
    pub(crate) digest: [u8; 32],
}

impl Material {
    pub(crate) fn load(
        files: &IdentityFiles,
        provider: Arc<CryptoProvider>,
        serves: &[Vec<u8>],
    ) -> Result<Self, IdentityError> {
        let certificate_pem = read(&files.certificate, FileRole::Certificate)?;
        let key_pem = read(&files.private_key, FileRole::PrivateKey)?;
        let trust_root_pem = read(&files.trust_root, FileRole::TrustRoot)?;

        let digest = digest_of(&[&certificate_pem, &key_pem, &trust_root_pem]);

        let chain = certificates(&certificate_pem, &files.certificate, FileRole::Certificate)?;
        let key = private_key(&key_pem, files)?;

        let mut roots = RootCertStore::empty();
        for anchor in certificates(&trust_root_pem, &files.trust_root, FileRole::TrustRoot)? {
            roots
                .add(anchor)
                .map_err(|source| IdentityError::UnusableTrustRoot {
                    path: files.trust_root.clone(),
                    source,
                })?;
        }
        let roots = Arc::new(roots);

        // The end-entity certificate is the first in the chain, by definition
        // of a chain: everything after it is there to get a verifier from it up
        // to the root.
        let id = name_in_certificate(&chain[0])?;

        let peer_verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::clone(&roots), Arc::clone(&provider))
                .build()
                .map_err(|source| IdentityError::UnusableTrustRoot {
                    path: files.trust_root.clone(),
                    source: rustls::Error::General(source.to_string()),
                })?;

        let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()?
            .with_client_cert_verifier(Arc::new(VerifyPeerIsInTrustDomain::new(
                peer_verifier,
                id.trust_domain(),
            )))
            .with_single_cert(chain.clone(), key.clone_key())
            .map_err(|source| IdentityError::Mismatched {
                certificate: files.certificate.clone(),
                private_key: files.private_key.clone(),
                source,
            })?;
        // Set after the builder rather than through it: rustls exposes this as
        // a field, and the builder chain has no step for it.
        server.alpn_protocols = serves.to_vec();

        Ok(Self {
            id,
            chain,
            key,
            roots,
            server: Arc::new(server),
            provider,
            digest,
        })
    }

    /// A configuration for dialling exactly one other machine.
    ///
    /// Built per call rather than kept, so that a connection opened after a
    /// rotation uses the material in force at that moment and nothing has to
    /// remember to invalidate a cache.
    pub(crate) fn client_config(
        &self,
        expected_peer: &SpiffeId,
        speaking: &[Vec<u8>],
    ) -> Result<Arc<ClientConfig>, IdentityError> {
        let verifier = VerifyPeerIs::new(
            Arc::clone(&self.roots),
            expected_peer.clone(),
            Arc::clone(&self.provider),
        );
        let mut config = ClientConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_client_auth_cert(self.chain.clone(), self.key.clone_key())?;
        config.alpn_protocols = speaking.to_vec();
        Ok(Arc::new(config))
    }
}

fn read(path: &Path, role: FileRole) -> Result<Vec<u8>, IdentityError> {
    fs::read(path).map_err(|source| IdentityError::Unreadable {
        role,
        path: path.to_path_buf(),
        source,
    })
}

fn certificates(
    pem: &[u8],
    path: &Path,
    role: FileRole,
) -> Result<Vec<CertificateDer<'static>>, IdentityError> {
    let found = rustls_pemfile::certs(&mut &pem[..])
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| IdentityError::MalformedPem {
            role,
            path: path.to_path_buf(),
            source,
        })?;
    if found.is_empty() {
        return Err(IdentityError::NoPemBlock {
            role,
            path: path.to_path_buf(),
        });
    }
    Ok(found)
}

fn private_key(pem: &[u8], files: &IdentityFiles) -> Result<PrivateKeyDer<'static>, IdentityError> {
    rustls_pemfile::private_key(&mut &pem[..])
        .map_err(|source| IdentityError::MalformedPem {
            role: FileRole::PrivateKey,
            path: files.private_key.clone(),
            source,
        })?
        .ok_or_else(|| IdentityError::NoPemBlock {
            role: FileRole::PrivateKey,
            path: files.private_key.clone(),
        })
}

/// A digest over all three files, each preceded by its length.
///
/// The lengths are what stop two different sets of files from hashing alike by
/// moving bytes from the end of one file to the start of the next.
fn digest_of(contents: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in contents {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}
