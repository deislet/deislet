//! A certificate authority small enough to live in a test.
//!
//! Everything here exists so the tests can answer questions about real
//! handshakes: issue a certificate naming a machine, write the three files an
//! identity is loaded from, and hand back the paths.

use std::path::{Path, PathBuf};

use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};

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

    /// A certificate naming one machine, with its key. Both ends of a Deislet
    /// connection prove themselves, so every certificate is good for both.
    pub fn issue(&self, spiffe_id: &str) -> Issued {
        self.sign(vec![SanType::URI(
            Ia5String::try_from(spiffe_id.to_string()).expect("an ia5 string"),
        )])
    }

    /// A certificate that proves nothing about who holds it: it chains to this
    /// authority, and carries a host name where a machine name should be.
    pub fn issue_without_a_name(&self, dns_name: &str) -> Issued {
        self.sign(vec![SanType::DnsName(
            Ia5String::try_from(dns_name.to_string()).expect("an ia5 string"),
        )])
    }

    /// A certificate carrying two machine names, so that "which one is it" has
    /// no answer.
    pub fn issue_with_two_names(&self, first: &str, second: &str) -> Issued {
        self.sign(vec![
            SanType::URI(Ia5String::try_from(first.to_string()).expect("an ia5 string")),
            SanType::URI(Ia5String::try_from(second.to_string()).expect("an ia5 string")),
        ])
    }

    fn sign(&self, subject_alt_names: Vec<SanType>) -> Issued {
        let key = KeyPair::generate().expect("a key pair");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("leaf parameters");
        params.subject_alt_names = subject_alt_names;
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
