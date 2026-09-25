//! The throwaway certificate authority `deis dev` issues itself.
//!
//! # Why a local node needs any of this
//!
//! `deis dev` drives the node it starts: it applies a deployment, unloads an
//! application, asks for status. Those are management calls, and a node admits
//! a management call only from a caller that proved, during the TLS handshake,
//! a certificate naming that node's control plane. There is no fleet-wide
//! value that opens it instead - that was retired, and the whole point of
//! retiring it was that no string should open this door.
//!
//! So `deis dev` has to be a control plane, in the only sense the node checks:
//! it has to hold a certificate with the right name, signed by something the
//! node trusts. Nothing issues one for a laptop, so this issues its own.
//!
//! # What these certificates are and are not
//!
//! They are a self-signed root and two leaves under it, generated fresh, kept
//! in the dev run's own directory, and thrown away with it. The root is
//! trusted by exactly one process - the node this same command started, in
//! this same directory, listening on loopback - and by nothing else on the
//! machine or anywhere near it.
//!
//! They are not how a real installation gets its certificates, and nothing
//! here should be read as a suggestion that it is. A real one has an authority
//! with a life longer than one command, and `deis-identity` deliberately knows
//! nothing about where the three files came from, which is exactly what makes
//! substituting a real authority free. See `deis-control.example.toml` for the
//! shape an installation configures.
//!
//! # The trust domain
//!
//! `deis.dev`, and deliberately not any name a real installation would use. A
//! certificate minted here names a machine of a trust domain that exists only
//! inside one `deis dev` run, so if one of these files were ever handed to a
//! real node it would be refused for naming a domain that node does not
//! belong to - which is a better failure than being accepted.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};

/// The trust domain every name in a `deis dev` run belongs to.
pub const DEV_TRUST_DOMAIN: &str = "deis.dev";

/// The node id `deis dev` starts its node under.
pub const DEV_NODE_ID: &str = "deis-dev";

/// One machine's three files, as the node's configuration and this CLI's
/// dialler both want them.
pub struct Machine {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub trust_root: PathBuf,
}

/// What one `deis dev` run issues itself: a node's identity for the process it
/// starts, and a control plane's identity for this CLI.
pub struct DevPki {
    pub node: Machine,
    pub control: Machine,
    /// The name the node will demand of whoever drives it, which is the name
    /// on `control`. Passed to the node rather than derived there, so the two
    /// sides cannot disagree about it.
    pub control_name: String,
}

/// Issue an authority and two machines under `directory`.
///
/// The private keys are written owner-readable. On a shared machine another
/// user who could read the node's key could drive that node, and while the
/// node only serves loopback, a key that is readable for no reason is a key
/// that will eventually be copied somewhere it matters.
pub fn issue(directory: &Path) -> Result<DevPki> {
    fs::create_dir_all(directory).with_context(|| {
        format!(
            "creating {} for this run's certificates",
            directory.display()
        )
    })?;

    let authority = Authority::new()?;
    let control_name = format!("spiffe://{DEV_TRUST_DOMAIN}/deislet/control");
    let node_name = format!("spiffe://{DEV_TRUST_DOMAIN}/deislet/node/{DEV_NODE_ID}");

    let root = directory.join("root.pem");
    write(&root, &authority.root_pem, false)?;

    let node = authority.issue(directory, "node", &node_name, &root)?;
    let control = authority.issue(directory, "control", &control_name, &root)?;

    Ok(DevPki {
        node,
        control,
        control_name,
    })
}

struct Authority {
    params: CertificateParams,
    key: KeyPair,
    root_pem: String,
}

impl Authority {
    fn new() -> Result<Self> {
        let key = KeyPair::generate().context("generating this run's authority key")?;
        let mut params = CertificateParams::new(Vec::<String>::new())
            .context("preparing this run's authority")?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let root_pem = params
            .self_signed(&key)
            .context("signing this run's authority")?
            .pem();
        Ok(Self {
            params,
            key,
            root_pem,
        })
    }

    fn issue(&self, directory: &Path, name: &str, spiffe: &str, root: &Path) -> Result<Machine> {
        let key = KeyPair::generate().with_context(|| format!("generating the {name} key"))?;
        let mut params =
            CertificateParams::new(Vec::<String>::new()).context("preparing a certificate")?;
        params.subject_alt_names = vec![SanType::URI(
            Ia5String::try_from(spiffe.to_string()).context("the machine name as a URI")?,
        )];
        // Both usages on both leaves, because each side here is a server on
        // one connection and a client on another: the node serves its
        // management plane and would dial a control plane if it had one, and
        // this CLI dials that plane while proving itself.
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let issuer = Issuer::from_params(&self.params, &self.key);
        let certificate = params
            .signed_by(&key, &issuer)
            .with_context(|| format!("signing the {name} certificate"))?;

        let machine = Machine {
            certificate: directory.join(format!("{name}.crt.pem")),
            private_key: directory.join(format!("{name}.key.pem")),
            trust_root: root.to_path_buf(),
        };
        write(&machine.certificate, &certificate.pem(), false)?;
        write(&machine.private_key, &key.serialize_pem(), true)?;
        Ok(machine)
    }
}

fn write(path: &Path, contents: &str, private: bool) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    if private {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restricting {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the pair: the node has to accept exactly the name
    /// the CLI will prove, and both have to chain to the one root. A run where
    /// the two disagreed would fail at the handshake with a message about a
    /// name, which reads like a bug in the platform rather than in this file.
    #[test]
    fn the_two_machines_share_a_root_and_the_control_name_is_the_one_issued() {
        let dir = tempfile::tempdir().expect("a directory");
        let pki = issue(dir.path()).expect("certificates");

        assert_eq!(pki.node.trust_root, pki.control.trust_root);
        let root = fs::read_to_string(&pki.node.trust_root).expect("the root");
        assert!(root.contains("BEGIN CERTIFICATE"));

        assert_eq!(
            pki.control_name,
            format!("spiffe://{DEV_TRUST_DOMAIN}/deislet/control")
        );
        // Parsed rather than string-compared, so this fails if the shape stops
        // being one `deis-identity` accepts.
        let parsed = deis_identity::SpiffeId::parse(&pki.control_name).expect("a machine name");
        assert_eq!(parsed.trust_domain(), DEV_TRUST_DOMAIN);
    }

    /// A key another user on the machine can read is a key that can drive this
    /// node. The node only listens on loopback, so this is not the last line
    /// of defence - which is exactly why it is worth having while it is cheap.
    #[cfg(unix)]
    #[test]
    fn a_private_key_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("a directory");
        let pki = issue(dir.path()).expect("certificates");
        for key in [&pki.node.private_key, &pki.control.private_key] {
            let mode = fs::metadata(key).expect("the key").permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "{} is {:o}",
                key.display(),
                mode & 0o777
            );
        }
    }
}
