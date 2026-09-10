//! Machine identity for the channel Deislet's own processes talk on.
//!
//! # What this is for
//!
//! The control plane, the runtime nodes and the proxy have to recognise each
//! other. Until now they did it with `node_preshared_keys`: one secret, shared
//! by the whole fleet, presented as a bearer token. Any holder of that one
//! value could register as any node, pull every application's decrypted
//! secrets, download any bundle, read the whole routing table and drive a
//! node's management plane as if it were the control plane. A copy of a
//! configuration file, a backup, or one compromised node was therefore a copy
//! of the entire installation's machine authority.
//!
//! This crate replaces the shared secret with something each process can only
//! speak for itself: a certificate naming exactly one machine, proved on every
//! connection by mutual TLS.
//!
//! # The contract, and why it is this narrow
//!
//! A process using this needs four things and no more:
//!
//! * a PEM file holding its own certificate chain;
//! * a PEM file holding the matching private key;
//! * a PEM file holding the trust root every peer must chain to;
//! * a reload when those files change on disk.
//!
//! Nothing here issues a certificate, renews one, or distributes a revocation.
//! That is a certificate authority's work, and keeping it outside is the whole
//! point: an installation can meet this contract with a CA built into Deislet,
//! with step-ca, with SPIRE, or with a shell script and `openssl`, and the code
//! on both sides of the connection is identical either way. What Deislet
//! insists on is the shape of the identity - one name, `spiffe://...`, proved
//! by a certificate - not where the certificate came from.
//!
//! # What it does not do
//!
//! It answers "who is calling", never "what may this caller do". A verified
//! connection means the peer proved a name in our trust domain; deciding that
//! this particular name may fetch that particular application's secrets is a
//! decision that needs to see the request, and so it belongs at the routes.
//! Splitting it this way is deliberate: a single place that answered both
//! questions would have to be consulted correctly by every route to be worth
//! anything, and a route that forgot would fail open.
//!
//! # Where to start
//!
//! [`MachineIdentity::load`] reads the three files. [`MachineIdentity::server_config`]
//! configures a listener that will not complete a handshake with a peer that
//! cannot prove a name; [`MachineIdentity::client_config`] configures a dialler
//! that will not talk to anybody but the one machine it meant to reach.
//! [`name_presented_by`] reads the peer's name off a finished connection, which
//! is what a route consults.

mod cert;
mod error;
mod material;
pub mod names;
mod spiffe;
/// A certificate authority small enough to hand a test.
///
mod verify;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::CertificateDer;
use rustls::{ClientConfig, ServerConfig};
use tracing::{info, warn};

pub use crate::cert::name_in_certificate;
pub use crate::error::{FileRole, IdentityError};
pub use crate::spiffe::{SpiffeId, SpiffeIdError, check_path_segment};

use crate::material::Material;

/// Where the three files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityFiles {
    /// This process's own certificate chain, end-entity certificate first.
    pub certificate: PathBuf,
    /// The private key belonging to that certificate.
    pub private_key: PathBuf,
    /// The certificate, or certificates, every peer must chain to.
    pub trust_root: PathBuf,
}

impl IdentityFiles {
    pub fn new(
        certificate: impl Into<PathBuf>,
        private_key: impl Into<PathBuf>,
        trust_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            certificate: certificate.into(),
            private_key: private_key.into(),
            trust_root: trust_root.into(),
        }
    }
}

/// This process's identity on the machine channel, and the material behind it.
///
/// Cheap to clone by `Arc` and safe to share: every read takes whatever
/// material is in force at that instant, so a rotation is invisible to callers
/// beyond the fact that connections made after it use the new certificate.
#[derive(Debug)]
pub struct MachineIdentity {
    files: IdentityFiles,
    current: ArcSwap<Material>,
}

impl MachineIdentity {
    /// Read the three files and build everything that depends on them.
    ///
    /// Fails rather than starting degraded. A process that cannot prove who it
    /// is has nothing useful to do on this channel, and starting anyway would
    /// only move the failure to the first connection - by which time it is an
    /// outage being diagnosed from a handshake error instead of a startup
    /// message naming the file.
    pub fn load(files: IdentityFiles) -> Result<Self, IdentityError> {
        let material = Material::load(&files, default_provider())?;
        Ok(Self {
            files,
            current: ArcSwap::from_pointee(material),
        })
    }

    /// The name this process proves to its peers.
    pub fn id(&self) -> SpiffeId {
        self.current.load().id.clone()
    }

    /// The files this identity was loaded from.
    pub fn files(&self) -> &IdentityFiles {
        &self.files
    }

    /// A listener configuration that requires the peer to prove a name in our
    /// trust domain before the connection exists at all.
    pub fn server_config(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.current.load().server)
    }

    /// A dialler configuration for reaching exactly one other machine.
    ///
    /// The address is not the identity: this will refuse to talk to whatever
    /// answers at the address unless it proves it is `expected_peer`.
    pub fn client_config(
        &self,
        expected_peer: &SpiffeId,
    ) -> Result<Arc<ClientConfig>, IdentityError> {
        self.current.load().client_config(expected_peer)
    }

    /// Re-read the files, and adopt them if they are usable and new.
    ///
    /// Three outcomes, and the middle one is the important one: if the files
    /// have changed into something that does not load - half-written, mutually
    /// mismatched, expired root - this returns the error and **leaves the
    /// material already in force untouched**. A rotation that goes wrong must
    /// not be able to take a running fleet's authentication down with it.
    pub fn reload(&self) -> Result<Reload, IdentityError> {
        let previous = self.current.load_full();
        let candidate = Material::load(&self.files, Arc::clone(&previous.provider))?;
        if candidate.digest == previous.digest {
            return Ok(Reload::Unchanged);
        }
        let outcome = Reload::Replaced {
            from: previous.id.clone(),
            to: candidate.id.clone(),
        };
        self.current.store(Arc::new(candidate));
        Ok(outcome)
    }

    /// Keep the material in step with the files, checking every `interval`.
    ///
    /// # Why this looks rather than listens
    ///
    /// A rotation writes three files, in an order nobody has promised. A
    /// filesystem watcher would fire once per file and so would see the set
    /// mid-rotation every time, which means it would need to wait for things to
    /// settle before it acted - and a watcher that waits before acting is a
    /// poll with more moving parts. Looking on a timer, and adopting only a set
    /// that loads as a whole, has the same worst case and no debounce to get
    /// wrong.
    ///
    /// The interval is the longest a rotation can go unnoticed, so it wants to
    /// be far shorter than the shortest certificate lifetime in use, and that
    /// is the only thing it trades against.
    pub fn watch(self: &Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        let identity = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick of a tokio interval completes immediately, and the
            // files were just read; skip it so the first real look is one
            // interval from now.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                identity.reload_and_report();
            }
        })
    }

    /// One look, with the outcome written where an operator will find it.
    fn reload_and_report(&self) {
        match self.reload() {
            Ok(Reload::Unchanged) => {}
            Ok(Reload::Replaced { from, to }) if from == to => {
                info!(
                    identity = %to,
                    certificate = %self.files.certificate.display(),
                    "adopted rotated identity material; connections made from now on use the new certificate and existing ones are left alone"
                );
            }
            Ok(Reload::Replaced { from, to }) => {
                warn!(
                    was = %from,
                    now = %to,
                    certificate = %self.files.certificate.display(),
                    "the certificate on disk now names a different machine, so this process has changed identity mid-run; anything that authorised the previous name will refuse this one until it is told about the change"
                );
            }
            Err(error) => {
                warn!(
                    error = %error,
                    identity = %self.id(),
                    "the identity files could not be re-read, so the material already in force stays in use; if this keeps failing until the current certificate expires, every connection on the machine channel will start failing at once"
                );
            }
        }
    }
}

/// What a [`reload`] found.
///
/// [`reload`]: MachineIdentity::reload
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reload {
    /// The files are byte for byte what they were.
    Unchanged,
    /// New material was adopted. `from` and `to` are equal for an ordinary
    /// renewal, and differ only if the certificate now names another machine.
    Replaced { from: SpiffeId, to: SpiffeId },
}

/// The name the far end of a finished connection proved.
///
/// Give it whatever the TLS connection reports as the peer's certificates -
/// `ServerConnection::peer_certificates`, or the equivalent on whatever server
/// wraps it. `None` and an empty list are the same thing and both refuse: on a
/// listener configured by [`MachineIdentity::server_config`] a connection
/// without a peer certificate cannot exist, so seeing one means the caller is
/// asking about the wrong connection.
pub fn name_presented_by(
    peer_certificates: Option<&[CertificateDer<'_>]>,
) -> Result<SpiffeId, IdentityError> {
    match peer_certificates.and_then(<[CertificateDer<'_>]>::first) {
        Some(end_entity) => name_in_certificate(end_entity),
        None => Err(IdentityError::NoPeerCertificate),
    }
}

/// The process-wide rustls crypto provider, installing the default if the
/// process has not chosen one.
///
/// Every configuration this crate builds is pinned to the provider that was in
/// force when its material was loaded, so a provider installed later by another
/// part of the process cannot silently change what a running listener accepts.
fn default_provider() -> Arc<CryptoProvider> {
    if let Some(installed) = CryptoProvider::get_default() {
        return Arc::clone(installed);
    }
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    // Losing the race is fine and expected: it means another thread installed
    // one first, and the read below picks up whichever won.
    let _ = CryptoProvider::install_default(provider.clone());
    CryptoProvider::get_default()
        .map(Arc::clone)
        .unwrap_or_else(|| Arc::new(provider))
}
