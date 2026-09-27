//! A certificate authority small enough to live in a test.
//!
//! Everything here exists so a test can answer questions about real
//! handshakes: issue a certificate naming a machine, write the three PEM
//! files an identity is loaded from, and hand back the paths. It is reachable
//! from any crate in this workspace, not only this one, because the shape of
//! a certificate that will actually load is easy to get wrong in a way that
//! only shows up as a baffling failure inside
//! [`deis_identity::MachineIdentity::load`]: no URI subject alternative name,
//! a name that is not `spiffe://`, two authorities that do not chain. Getting
//! it right once here is cheaper than every harness that needs mutual TLS
//! discovering the same mistakes on its own.
//!
//! # Why this is a crate of its own
//!
//! It was first written inside `deis-identity`, behind a `test-pki` cargo
//! feature. That did not hold the line it claimed to. Cargo unifies features
//! across a workspace build, so the moment any crate's dev-dependency turned
//! the feature on, every other crate's *library* code was compiled against a
//! `deis-identity` that had a certificate authority in it - and code that
//! reached for it would pass `cargo test --workspace` and
//! `cargo clippy --workspace --all-targets`, failing only at a later plain
//! `cargo build`.
//!
//! A separate crate makes the boundary structural instead of documented: a
//! process that ships does not depend on this crate, so there is no feature
//! for a build to unify and nothing to reach for. The dependency runs the
//! other way - this crate depends on `deis-identity`, and a harness takes it
//! as a dev-dependency.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};

use deis_identity::SpiffeId;

/// A test certificate authority: a root, and the ability to sign for it.
pub struct TestCa {
    params: CertificateParams,
    key: KeyPair,
    pub root_pem: String,
}

impl TestCa {
    pub fn new() -> Self {
        let key = KeyPair::generate().expect("a key pair");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("ca parameters");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let root_pem = params.self_signed(&key).expect("a root certificate").pem();
        Self {
            params,
            key,
            root_pem,
        }
    }

    /// A certificate naming one machine, with its key.
    ///
    /// Takes a [`SpiffeId`] rather than a bare string so a caller building one
    /// out of [`deis_identity::names`] - the spelling this fleet actually issues -
    /// gets it for free instead of retyping the shape by hand. For a name
    /// that must not be well-formed, or that comes from outside this crate's
    /// own rules, use [`Self::issue_named`].
    pub fn issue(&self, spiffe_id: &SpiffeId) -> Issued {
        self.issue_named(spiffe_id.as_str())
    }

    /// A certificate naming one machine, from a URI this does not require to
    /// be a well-formed SPIFFE identity.
    ///
    /// This is the escape hatch [`Self::issue`] does not offer: a test that
    /// wants to see how this crate reacts to a name it would itself refuse -
    /// wrong scheme, wrong trust domain, an alias [`deis_identity::SpiffeId`] does not
    /// accept - has to be able to put that name on a certificate in the first
    /// place, which means going around [`SpiffeId::parse`] rather than through
    /// it.
    pub fn issue_named(&self, spiffe_id: &str) -> Issued {
        self.sign(spiffe_id, vec![uri_san(spiffe_id)])
    }

    /// A certificate that proves nothing about who holds it: it chains to this
    /// authority, and carries a host name where a machine name should be.
    pub fn issue_without_a_name(&self, dns_name: &str) -> Issued {
        self.sign(
            dns_name,
            vec![SanType::DnsName(
                Ia5String::try_from(dns_name.to_string()).expect("an ia5 string"),
            )],
        )
    }

    /// A certificate carrying two machine names, so that "which one is it" has
    /// no answer.
    pub fn issue_with_two_names(&self, first: &str, second: &str) -> Issued {
        self.sign(first, vec![uri_san(first), uri_san(second)])
    }

    /// A certificate naming one machine, with additional subject alternative
    /// names beyond the SPIFFE URI - typically an IP address.
    ///
    /// This is a harness concession, not a product requirement: mutual TLS on
    /// this channel authenticates by SPIFFE name alone, so a certificate a
    /// real deployment issues has no reason to carry a host name or an
    /// address. But a test harness that drives a connection through something
    /// like `curl` verifies the server side by hostname or address the
    /// ordinary way, before this crate's own check of the SPIFFE name ever
    /// runs - so a leaf with only a URI SAN fails that first check, and
    /// turning hostname verification off to route around it would stop the
    /// probe from proving what it set out to prove. Giving the leaf an IP SAN
    /// as well lets the probe verify normally while this crate still
    /// authenticates the connection by the URI SAN alone.
    pub fn issue_with_addresses(&self, spiffe_id: &SpiffeId, addresses: &[IpAddr]) -> Issued {
        let mut subject_alt_names = vec![uri_san(spiffe_id.as_str())];
        subject_alt_names.extend(addresses.iter().copied().map(SanType::IpAddress));
        self.sign(spiffe_id.as_str(), subject_alt_names)
    }

    /// A certificate naming one machine, valid from a minute ago for
    /// `lifetime`.
    ///
    /// Every other method here issues rcgen's default window, which runs for
    /// centuries; this is for the tests about how long a certificate is.
    pub fn issue_for(&self, spiffe_id: &SpiffeId, lifetime: std::time::Duration) -> Issued {
        let not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
        let lifetime = time::Duration::try_from(lifetime).expect("a lifetime that fits");
        self.sign_valid(
            spiffe_id.as_str(),
            vec![uri_san(spiffe_id.as_str())],
            Some((not_before, not_before + lifetime)),
        )
    }

    fn sign(&self, holder: &str, subject_alt_names: Vec<SanType>) -> Issued {
        self.sign_valid(holder, subject_alt_names, None)
    }

    fn sign_valid(
        &self,
        holder: &str,
        subject_alt_names: Vec<SanType>,
        window: Option<(time::OffsetDateTime, time::OffsetDateTime)>,
    ) -> Issued {
        let key = KeyPair::generate().expect("a key pair");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("leaf parameters");
        if let Some((not_before, not_after)) = window {
            params.not_before = not_before;
            params.not_after = not_after;
        }
        params.subject_alt_names = subject_alt_names;
        // A subject of this leaf's own, distinct from the root's. rcgen would
        // otherwise stamp every certificate it makes - the authority included
        // - with one shared subject, and a leaf whose subject names its issuer
        // is indistinguishable from a self-signed one to a verifier that
        // matches by name: openssl refuses such a leaf with UnknownCA where
        // rustls, which reads the key identifiers, accepts it. A real
        // authority never signs two certificates with one subject either.
        let mut distinguished_name = rcgen::DistinguishedName::new();
        distinguished_name.push(rcgen::DnType::CommonName, holder);
        params.distinguished_name = distinguished_name;
        // Every process in this fleet dials some peers and accepts connections
        // from others - the control plane serves nodes and proxies but also
        // dials out to them for management calls, and a node likewise accepts
        // from the control plane while proving itself as a client wherever it
        // calls out. A leaf good for only one of the two extended key usages
        // would work in half of this crate's own tests and fail the other
        // half for a reason that has nothing to do with what is under test.
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let issuer = Issuer::from_params(&self.params, &self.key);
        let certificate = params.signed_by(&key, &issuer).expect("a leaf certificate");
        Issued {
            certificate_pem: certificate.pem(),
            key_pem: key.serialize_pem(),
        }
    }
}

impl Default for TestCa {
    fn default() -> Self {
        Self::new()
    }
}

fn uri_san(value: &str) -> SanType {
    SanType::URI(Ia5String::try_from(value.to_string()).expect("an ia5 string"))
}

pub struct Issued {
    pub certificate_pem: String,
    pub key_pem: String,
}

/// The three paths an identity is loaded from, under one directory.
pub struct WrittenFiles {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub trust_root: PathBuf,
}

impl WrittenFiles {
    pub fn as_identity_files(&self) -> deis_identity::IdentityFiles {
        deis_identity::IdentityFiles::new(
            self.certificate.clone(),
            self.private_key.clone(),
            self.trust_root.clone(),
        )
    }
}

/// Write one machine's three files into `directory`, under `name` so that two
/// machines can share a directory.
pub fn write_files(directory: &Path, name: &str, issued: &Issued, root_pem: &str) -> WrittenFiles {
    let files = WrittenFiles {
        certificate: directory.join(format!("{name}.crt.pem")),
        private_key: directory.join(format!("{name}.key.pem")),
        trust_root: directory.join(format!("{name}.root.pem")),
    };
    std::fs::write(&files.certificate, &issued.certificate_pem).expect("write certificate");
    std::fs::write(&files.private_key, &issued.key_pem).expect("write key");
    std::fs::write(&files.trust_root, root_pem).expect("write trust root");
    files
}

/// One authority and a directory to write its certificates into: what a test
/// of a data service - deis-store, deis-queue, deis-telemetry - needs to
/// serve over TLS and dial it the way a real caller does.
///
/// The directory is removed when the fleet is dropped, so keep the fleet
/// alive for as long as anything loaded from it is.
pub struct TestFleet {
    pub ca: TestCa,
    pub trust_domain: String,
    dir: tempfile::TempDir,
}

impl TestFleet {
    pub fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("a directory for the test fleet");
        let ca = TestCa::new();
        std::fs::write(dir.path().join("ca.crt.pem"), &ca.root_pem).expect("write trust root");
        Self {
            ca,
            trust_domain: "deis.test".to_string(),
            dir,
        }
    }

    /// The directory the fleet's files are written into.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// The fleet's trust root, on disk.
    pub fn trust_root(&self) -> PathBuf {
        self.dir.path().join("ca.crt.pem")
    }

    /// Issue `id` a certificate and write its three files, named after `name`.
    pub fn files_for(&self, name: &str, id: &SpiffeId) -> WrittenFiles {
        write_files(self.dir.path(), name, &self.ca.issue(id), &self.ca.root_pem)
    }

    /// Issue `id` a certificate and load it as a machine identity serving
    /// `serves`.
    pub fn identity(
        &self,
        name: &str,
        id: &SpiffeId,
        serves: &[deis_identity::Protocol],
    ) -> std::sync::Arc<deis_identity::MachineIdentity> {
        let files = self.files_for(name, id);
        std::sync::Arc::new(
            deis_identity::MachineIdentity::load(files.as_identity_files(), serves)
                .expect("a test identity loads"),
        )
    }

    /// The identity a data service of `kind` serves with: named
    /// `spiffe://deis.test/deislet/<kind>/test`, speaking HTTP/2.
    pub fn service_identity(
        &self,
        kind: deis_identity::ServiceKind,
    ) -> std::sync::Arc<deis_identity::MachineIdentity> {
        use deis_identity::{ServiceKind, names};
        let (name, id) = match kind {
            ServiceKind::Store => ("store", names::store(&self.trust_domain, "test")),
            ServiceKind::Queue => ("queue", names::queue(&self.trust_domain, "test")),
            ServiceKind::Telemetry => ("telemetry", names::telemetry(&self.trust_domain, "test")),
            ServiceKind::Control => ("control", names::control(&self.trust_domain)),
            ServiceKind::Node => ("node", names::node(&self.trust_domain, "test")),
            ServiceKind::Proxy => ("proxy", names::proxy(&self.trust_domain, "test")),
        };
        self.identity(
            name,
            &id.expect("a well-formed test name"),
            &[deis_identity::Protocol::Http2],
        )
    }

    /// What a caller holding only this fleet's trust root trusts - a CLI.
    pub fn service_trust(&self) -> deis_identity::ServiceTrust {
        deis_identity::ServiceTrust::from_trust_root(&self.trust_root())
            .expect("the test trust root loads")
    }
}

impl Default for TestFleet {
    fn default() -> Self {
        Self::new()
    }
}
