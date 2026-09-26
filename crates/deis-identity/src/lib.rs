//! Machine identity for the channel Deislet's own processes talk on.
//!
//! # What this is for
//!
//! The control plane, the runtime nodes and the proxy have to recognise each
//! other. They used to do it with `node_preshared_keys`: one secret, shared by
//! the whole installation, presented as a bearer token. Any holder of that one
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
pub mod dial;
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
use tracing::{error, info, warn};

pub use crate::cert::name_in_certificate;
pub use crate::dial::Dialler;
pub use crate::error::{FileRole, IdentityError};
pub use crate::names::ServiceKind;
pub use crate::spiffe::{SpiffeId, SpiffeIdError, check_path_segment};

use crate::material::Material;

/// A protocol a TLS connection on this channel may carry.
///
/// # Why this has to be said out loud
///
/// TLS negotiates the application protocol during the handshake, and a peer
/// that offers `h2` to a listener advertising nothing gets a connection with
/// no agreed protocol. What happens next depends on the stack: hyper's server
/// sniffs the HTTP/2 connection preface off the byte stream and carries on, so
/// this fleet works today without ever setting it - but that is an accident of
/// one implementation, not an agreement. Put a strict client, a load balancer
/// or a service mesh in the path and the connection completes and then behaves
/// in a way nobody can explain from either log.
///
/// So the protocols are stated. The machine listener carries gRPC and a bundle
/// download, which is both of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP/2, which is what gRPC runs on.
    Http2,
    /// HTTP/1.1, which is what the bundle download is.
    Http11,
}

impl Protocol {
    /// The identifier as it goes on the wire, from the IANA registry.
    pub fn wire_name(self) -> &'static [u8] {
        match self {
            Self::Http2 => b"h2",
            Self::Http11 => b"http/1.1",
        }
    }

    /// The list rustls wants, in the caller's order of preference.
    fn wire_names(protocols: &[Self]) -> Vec<Vec<u8>> {
        protocols.iter().map(|p| p.wire_name().to_vec()).collect()
    }
}

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
    /// Bumped every time new material is adopted.
    ///
    /// Anything that took a copy of [`Self::server_config`] holds a snapshot
    /// of the material in force when it asked, and a listener built from one
    /// would go on presenting the certificate the process started with - past
    /// a renewal, and up to the moment the old certificate expired, when
    /// every handshake would fail at once. A dialler avoids that by resolving
    /// per connection; a listener cannot, because the configuration is handed
    /// to the serving stack before any connection exists. So it is told
    /// instead: see [`Self::renewals`].
    generation: tokio::sync::watch::Sender<u64>,
    /// What this process's listener advertises, kept so a reload rebuilds the
    /// server configuration with the same answer it started with.
    serves: Vec<Vec<u8>>,
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
    /// `serves` is what this process's own listener will carry, in order of
    /// preference. It belongs here rather than on [`Self::server_config`]
    /// because it is fixed for the life of the process, and a listener whose
    /// advertised protocols could change under it would be a listener no
    /// peer's cached connection could rely on. What a *dialler* wants is a
    /// different question and is answered per call - see
    /// [`Self::client_config`].
    ///
    /// A process that only dials passes an empty slice.
    pub fn load(files: IdentityFiles, serves: &[Protocol]) -> Result<Self, IdentityError> {
        let serves = Protocol::wire_names(serves);
        let material = Material::load(&files, default_provider(), &serves)?;
        Ok(Self {
            files,
            serves,
            generation: tokio::sync::watch::Sender::new(0),
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

    /// Seconds since the epoch, as the expiry report measures them.
    ///
    /// A saturating conversion rather than an unwrap: a clock set before 1970
    /// is a broken machine, and a panic in a background watcher would take the
    /// reload with it - which is the one thing that still has to keep working
    /// on a machine whose clock is wrong.
    fn now_unix() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }

    /// When the certificate currently in force stops being valid.
    pub fn validity(&self) -> crate::cert::Validity {
        self.current.load().validity
    }

    /// State when the certificate in force expires, whether or not that is
    /// soon.
    ///
    /// Separate from [`Self::report_expiry`] because the two answer different
    /// questions. This one is a fact an operator should be able to find in the
    /// log of a healthy process; that one is a call to act. Merging them would
    /// mean either saying nothing while everything is fine - leaving nowhere
    /// to look up the date - or warning on every correct startup.
    pub fn announce_expiry(&self, now: i64) {
        let validity = self.validity();
        info!(
            identity = %self.id(),
            certificate = %self.files.certificate.display(),
            remaining_secs = validity.remaining_at(now),
            lifetime_secs = validity.not_after - validity.not_before,
            "This machine's certificate is good for that long. Nothing here renews it; when it \
             expires, every connection on the machine channel fails at once"
        );
    }

    /// Report how much of the current certificate's life is left, at whatever
    /// level that deserves.
    ///
    /// # Why this exists at all
    ///
    /// Nothing in Deislet issues, renews or revokes a certificate - that is
    /// the installation's own authority, and keeping it out is what lets any
    /// PKI be substituted for any other. The cost of that decision is that
    /// expiry is somebody else's job and nothing here would notice it being
    /// forgotten. An expiring certificate is invisible right up to the moment
    /// it expires, and then every connection on the machine channel fails at
    /// once: nodes stop fetching assignments, the control plane stops being
    /// able to drive them, and the proxy stops seeing route changes - all of
    /// it at the same instant, for a reason none of those processes will name.
    ///
    /// So this crate cannot fix expiry and must not be silent about it.
    ///
    /// # Why the threshold is a fraction and not a number of days
    ///
    /// Deislet is handed both two-day certificates (its own demo) and
    /// multi-year ones. A week's notice is early on one and already too late
    /// on the other, and a fixed threshold would either say nothing useful or
    /// fire on every correct startup - which teaches an operator that this
    /// crate's warnings are noise, on the way to the one that is not. Measured
    /// against the certificate's own lifetime, one rule fits every issuer and
    /// a freshly issued certificate never trips it.
    pub fn report_expiry(&self, now: i64) {
        let validity = self.validity();
        let spent = validity.spent_by(now);
        let remaining = validity.remaining_at(now);
        let id = self.id();
        let certificate = self.files.certificate.display();
        if remaining <= 0 {
            error!(
                identity = %id,
                certificate = %certificate,
                expired_secs_ago = -remaining,
                "This machine's certificate has expired. Every connection it makes or accepts on \
                 the machine channel is refused during the handshake, so this process can neither \
                 reach its peers nor be reached by them. Nothing here can renew it: issue a new \
                 certificate for this name and write it to the same path, and it is picked up \
                 without a restart"
            );
        } else if spent >= 0.95 {
            error!(
                identity = %id,
                certificate = %certificate,
                remaining_secs = remaining,
                "This machine's certificate is in the last twentieth of its life. When it \
                 expires, every connection on the machine channel starts failing at the same \
                 instant - this process will neither reach its peers nor be reachable. Issue a \
                 replacement for this name at the same path; it is picked up without a restart"
            );
        } else if spent >= 0.80 {
            warn!(
                identity = %id,
                certificate = %certificate,
                remaining_secs = remaining,
                "This machine's certificate has used four fifths of its life. Nothing here renews \
                 it, and when it expires every connection on the machine channel fails at once. \
                 Issue a replacement for this name at the same path; it is picked up without a \
                 restart"
            );
        }
    }

    /// A listener configuration that requires the peer to prove a name in our
    /// trust domain before the connection exists at all.
    pub fn server_config(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.current.load().server)
    }

    /// A listener configuration that admits one kind of machine and no other.
    ///
    /// The data port of a runtime node is the reason this exists: it serves
    /// application traffic forwarded by this fleet's proxies, so a peer must
    /// prove not only a name in this trust domain but a proxy's name. A node's
    /// certificate - a machine the same authority signed, for a job on the
    /// same channel - is refused here, because being one of ours is not being
    /// the machine this listener takes traffic from.
    ///
    /// Unlike [`Self::server_config`] this is built per call rather than kept
    /// in the material: only listeners that exist serve it, and the ones this
    /// crate knows about are built once at load. A listener that takes this
    /// must follow [`Self::renewals`] exactly as one taking the plain
    /// configuration does.
    pub fn server_config_admitting(
        &self,
        kind: ServiceKind,
    ) -> Result<Arc<ServerConfig>, IdentityError> {
        self.current.load().server_config_of_kind(kind)
    }

    /// A signal that new material has been adopted.
    ///
    /// For the serving side, which cannot resolve per connection: the
    /// configuration is handed to the listener before any connection exists,
    /// so a listener has to be told when to take a fresh one. Every change
    /// bumps the value; a receiver that has been away sees one change rather
    /// than a queue of them, which is right - what matters is that the
    /// material is different now, not how many times it became so.
    ///
    /// A caller that ignores this is a caller whose listener will present the
    /// certificate this process started with for as long as it runs.
    pub fn renewals(&self) -> tokio::sync::watch::Receiver<u64> {
        self.generation.subscribe()
    }

    /// A dialler configuration for reaching exactly one other machine.
    ///
    /// The address is not the identity: this will refuse to talk to whatever
    /// answers at the address unless it proves it is `expected_peer`.
    /// `speaking` is what this particular connection is for, in order of
    /// preference: `Http2` to open a gRPC channel, `Http11` to fetch a
    /// bundle. Per call and not per identity, because one process does both
    /// over the same certificate.
    pub fn client_config(
        &self,
        expected_peer: &SpiffeId,
        speaking: &[Protocol],
    ) -> Result<Arc<ClientConfig>, IdentityError> {
        self.current
            .load()
            .client_config(expected_peer, &Protocol::wire_names(speaking))
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
        let candidate = Material::load(&self.files, Arc::clone(&previous.provider), &self.serves)?;
        if candidate.digest == previous.digest {
            return Ok(Reload::Unchanged);
        }
        let outcome = Reload::Replaced {
            from: previous.id.clone(),
            to: candidate.id.clone(),
        };
        self.current.store(Arc::new(candidate));
        // After the swap, so anything woken by this finds the new material
        // rather than racing the store it was told about.
        self.generation.send_modify(|generation| *generation += 1);
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
            // Said once at startup even when there is nothing wrong. An
            // operator reads the startup lines and rarely reads anything else
            // until something breaks, so the one moment this crate is
            // guaranteed an audience is the moment to say when somebody has to
            // act - not the moment it has become urgent.
            identity.announce_expiry(Self::now_unix());
            identity.report_expiry(Self::now_unix());
            loop {
                ticker.tick().await;
                identity.reload_and_report();
                // Checked on the same tick as the reload, because the two
                // questions are one: a renewal that happened is what stops
                // this from having anything to say.
                identity.report_expiry(Self::now_unix());
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
