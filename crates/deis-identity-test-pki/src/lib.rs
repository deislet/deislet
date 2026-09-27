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

    fn sign(&self, holder: &str, subject_alt_names: Vec<SanType>) -> Issued {
        let key = KeyPair::generate().expect("a key pair");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("leaf parameters");
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
