//! DSSE-wrapped authorization for one exact provider driver release.
//!
//! The release index and archive digests prove internal integrity. This module
//! adds the missing publisher authorization boundary. It deliberately uses
//! DSSE v1 pre-authentication encoding and an out-of-band, threshold trust
//! root instead of inventing a private signature envelope.

use std::{collections::BTreeMap, fmt, fs::OpenOptions, io::Read as _, path::Path};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    Sha256Digest, ToolDigest, ValidatedProviderDriverManifest, ValidatedProviderDriverRelease,
};

pub const DSSE_ENVELOPE_PAYLOAD_TYPE: &str =
    "application/vnd.deislet.provider-driver-release-authorization.v1+json";

const MAX_SIGNING_KEYS: usize = 32;
const MAX_SIGNATURES: usize = 32;
const MAX_TRUST_ROOT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriverReleaseSigningKeyStatus {
    Active,
    Revoked,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseSigningKey {
    pub key_id: String,
    pub key_type: String,
    pub scheme: String,
    pub public_key: String,
    pub status: DriverReleaseSigningKeyStatus,
    pub valid_from: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseSigningRole {
    pub key_ids: Vec<String>,
    pub threshold: u32,
    pub max_authorization_lifetime_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseTrustRoot {
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    pub schema_version: u32,
    pub root_version: u64,
    pub expires_at: DateTime<Utc>,
    pub provider_driver_release: DriverReleaseSigningRole,
    pub keys: Vec<DriverReleaseSigningKey>,
}

#[derive(Clone, Debug)]
pub struct ValidatedDriverReleaseTrustRoot {
    root: DriverReleaseTrustRoot,
    verifying_keys: BTreeMap<String, VerifyingKey>,
    canonical_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseAuthorizationSubject {
    pub release_sha256: ToolDigest,
    pub manifest_sha256: ToolDigest,
    pub backend_id: String,
    pub driver_id: String,
    pub driver_version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseAuthorization {
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    pub schema_version: u32,
    pub subject: DriverReleaseAuthorizationSubject,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseDsseSignature {
    pub keyid: String,
    pub sig: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseDsseEnvelope {
    pub payload_type: String,
    pub payload: String,
    pub signatures: Vec<DriverReleaseDsseSignature>,
}

#[derive(Clone, Debug)]
pub struct ValidatedDriverReleaseAuthorization {
    envelope: DriverReleaseDsseEnvelope,
    authorization: DriverReleaseAuthorization,
    payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDriverReleaseAuthorization {
    pub trust_root_version: u64,
    pub trust_root_sha256: Sha256Digest,
    pub envelope_sha256: Sha256Digest,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub signer_key_ids: Vec<String>,
}

#[derive(Debug)]
pub enum DriverReleaseAuthorizationError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Invalid(&'static str),
}

impl fmt::Display for DriverReleaseAuthorizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(
                formatter,
                "cannot read provider driver release trust root: {error}"
            ),
            Self::Json(error) => write!(
                formatter,
                "invalid driver release authorization JSON: {error}"
            ),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DriverReleaseAuthorizationError {}

impl ValidatedDriverReleaseTrustRoot {
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self, DriverReleaseAuthorizationError> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "provider driver release trust root path must be absolute",
            ));
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        #[cfg(windows)]
        options
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
        let mut file = options
            .open(path)
            .map_err(DriverReleaseAuthorizationError::Io)?;
        let metadata = file
            .metadata()
            .map_err(DriverReleaseAuthorizationError::Io)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "provider driver release trust root must be a regular non-symlink file",
            ));
        }
        #[cfg(windows)]
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "provider driver release trust root cannot be a Windows reparse point",
            ));
        }
        if metadata.len() > MAX_TRUST_ROOT_BYTES {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "provider driver release trust root exceeds one MiB",
            ));
        }
        #[cfg(unix)]
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "provider driver release trust root must be owned by the service identity and not group/other writable",
            ));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        file.read_to_end(&mut bytes)
            .map_err(DriverReleaseAuthorizationError::Io)?;
        let final_metadata = file
            .metadata()
            .map_err(DriverReleaseAuthorizationError::Io)?;
        #[cfg(unix)]
        let metadata_changed = metadata.dev() != final_metadata.dev()
            || metadata.ino() != final_metadata.ino()
            || metadata.len() != final_metadata.len()
            || metadata.mtime() != final_metadata.mtime()
            || metadata.mtime_nsec() != final_metadata.mtime_nsec();
        #[cfg(not(unix))]
        let metadata_changed = metadata.len() != final_metadata.len();
        if metadata_changed || u64::try_from(bytes.len()).ok() != Some(final_metadata.len()) {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "provider driver release trust root changed while it was read",
            ));
        }
        Self::parse_json(&bytes)
    }

    pub fn parse_json(bytes: &[u8]) -> Result<Self, DriverReleaseAuthorizationError> {
        let root = serde_json::from_slice(bytes).map_err(DriverReleaseAuthorizationError::Json)?;
        Self::new(root)
    }

    pub fn new(root: DriverReleaseTrustRoot) -> Result<Self, DriverReleaseAuthorizationError> {
        validate_trust_root(&root)?;
        let mut verifying_keys = BTreeMap::new();
        for key in &root.keys {
            let bytes = decode_canonical_base64(&key.public_key)?;
            let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
                DriverReleaseAuthorizationError::Invalid(
                    "provider driver release Ed25519 public key must be 32 bytes",
                )
            })?;
            let verifying_key = VerifyingKey::from_bytes(&bytes).map_err(|_| {
                DriverReleaseAuthorizationError::Invalid(
                    "provider driver release Ed25519 public key is invalid",
                )
            })?;
            verifying_keys.insert(key.key_id.clone(), verifying_key);
        }
        let canonical = serde_jcs::to_vec(&root).map_err(DriverReleaseAuthorizationError::Json)?;
        Ok(Self {
            root,
            verifying_keys,
            canonical_sha256: sha256(&canonical),
        })
    }

    #[must_use]
    pub fn get(&self) -> &DriverReleaseTrustRoot {
        &self.root
    }

    #[must_use]
    pub fn canonical_sha256(&self) -> &Sha256Digest {
        &self.canonical_sha256
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, DriverReleaseAuthorizationError> {
        serde_jcs::to_vec(&self.root).map_err(DriverReleaseAuthorizationError::Json)
    }
}

impl ValidatedDriverReleaseAuthorization {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, DriverReleaseAuthorizationError> {
        let envelope =
            serde_json::from_slice(bytes).map_err(DriverReleaseAuthorizationError::Json)?;
        Self::new(envelope)
    }

    pub fn new(
        envelope: DriverReleaseDsseEnvelope,
    ) -> Result<Self, DriverReleaseAuthorizationError> {
        if envelope.payload_type != DSSE_ENVELOPE_PAYLOAD_TYPE {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "driver release authorization uses an unsupported DSSE payload type",
            ));
        }
        if envelope.signatures.is_empty() || envelope.signatures.len() > MAX_SIGNATURES {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "driver release authorization must contain one to 32 signatures",
            ));
        }
        let payload = decode_canonical_base64(&envelope.payload)?;
        let authorization: DriverReleaseAuthorization =
            serde_json::from_slice(&payload).map_err(DriverReleaseAuthorizationError::Json)?;
        validate_authorization(&authorization)?;
        let canonical =
            serde_jcs::to_vec(&authorization).map_err(DriverReleaseAuthorizationError::Json)?;
        if payload != canonical {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "driver release authorization payload must be RFC 8785 canonical JSON",
            ));
        }
        let mut previous_key_id: Option<&str> = None;
        for signature in &envelope.signatures {
            if !is_sha256_hex(&signature.keyid)
                || previous_key_id.is_some_and(|previous| previous >= signature.keyid.as_str())
            {
                return Err(DriverReleaseAuthorizationError::Invalid(
                    "driver release authorization signatures must have unique canonical key IDs in order",
                ));
            }
            previous_key_id = Some(&signature.keyid);
            if decode_canonical_base64(&signature.sig)?.len() != 64 {
                return Err(DriverReleaseAuthorizationError::Invalid(
                    "driver release authorization Ed25519 signature must be 64 bytes",
                ));
            }
        }
        Ok(Self {
            envelope,
            authorization,
            payload,
        })
    }

    #[must_use]
    pub fn get(&self) -> &DriverReleaseDsseEnvelope {
        &self.envelope
    }

    #[must_use]
    pub fn authorization(&self) -> &DriverReleaseAuthorization {
        &self.authorization
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, DriverReleaseAuthorizationError> {
        serde_jcs::to_vec(&self.envelope).map_err(DriverReleaseAuthorizationError::Json)
    }

    pub fn verify(
        &self,
        release: &ValidatedProviderDriverRelease,
        manifest: &ValidatedProviderDriverManifest,
        trust_root: &ValidatedDriverReleaseTrustRoot,
        now: DateTime<Utc>,
    ) -> Result<VerifiedDriverReleaseAuthorization, DriverReleaseAuthorizationError> {
        verify_subject(&self.authorization.subject, release, manifest)?;
        if now >= trust_root.root.expires_at {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "provider driver release trust root has expired",
            ));
        }
        if now < self.authorization.issued_at || now >= self.authorization.expires_at {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "provider driver release authorization is not currently valid",
            ));
        }
        let lifetime = self.authorization.expires_at - self.authorization.issued_at;
        let lifetime_seconds = lifetime.num_seconds();
        if lifetime_seconds <= 0
            || u64::try_from(lifetime_seconds).ok()
                > Some(
                    trust_root
                        .root
                        .provider_driver_release
                        .max_authorization_lifetime_seconds,
                )
        {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "provider driver release authorization exceeds the trust policy lifetime",
            ));
        }

        let pae = dsse_pae(&self.envelope.payload_type, &self.payload);
        let keys_by_id = trust_root
            .root
            .keys
            .iter()
            .map(|key| (key.key_id.as_str(), key))
            .collect::<BTreeMap<_, _>>();
        let role = &trust_root.root.provider_driver_release;
        let mut valid_signers = Vec::new();
        for dsse_signature in &self.envelope.signatures {
            if !role
                .key_ids
                .iter()
                .any(|key_id| key_id == &dsse_signature.keyid)
            {
                return Err(DriverReleaseAuthorizationError::Invalid(
                    "driver release authorization contains a signer outside the trusted role",
                ));
            }
            let key = keys_by_id.get(dsse_signature.keyid.as_str()).ok_or(
                DriverReleaseAuthorizationError::Invalid(
                    "driver release authorization signer is absent from the trust root",
                ),
            )?;
            if key.status != DriverReleaseSigningKeyStatus::Active
                || key.revoked_at.is_some()
                || self.authorization.issued_at < key.valid_from
                || self.authorization.issued_at >= key.valid_until
            {
                return Err(DriverReleaseAuthorizationError::Invalid(
                    "driver release authorization signer is revoked or was not valid at issuance",
                ));
            }
            let signature_bytes = decode_canonical_base64(&dsse_signature.sig)?;
            let signature = Signature::from_slice(&signature_bytes).map_err(|_| {
                DriverReleaseAuthorizationError::Invalid(
                    "driver release authorization Ed25519 signature is invalid",
                )
            })?;
            trust_root.verifying_keys[&dsse_signature.keyid]
                .verify(&pae, &signature)
                .map_err(|_| {
                    DriverReleaseAuthorizationError::Invalid(
                        "driver release authorization signature verification failed",
                    )
                })?;
            valid_signers.push(dsse_signature.keyid.clone());
        }
        if valid_signers.len() < usize::try_from(role.threshold).unwrap_or(usize::MAX) {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "driver release authorization does not satisfy the trusted signature threshold",
            ));
        }
        let envelope_json = self.canonical_json()?;
        Ok(VerifiedDriverReleaseAuthorization {
            trust_root_version: trust_root.root.root_version,
            trust_root_sha256: trust_root.canonical_sha256.clone(),
            envelope_sha256: sha256(&envelope_json),
            issued_at: self.authorization.issued_at,
            expires_at: self.authorization.expires_at,
            signer_key_ids: valid_signers,
        })
    }
}

fn validate_trust_root(
    root: &DriverReleaseTrustRoot,
) -> Result<(), DriverReleaseAuthorizationError> {
    require(
        root.schema
            .as_deref()
            .is_some_and(|schema| !schema.is_empty()),
        "provider driver release trust root must identify its JSON schema",
    )?;
    require(
        root.schema_version == 1 && root.root_version >= 1,
        "provider driver release trust root version is invalid",
    )?;
    require(
        !root.keys.is_empty() && root.keys.len() <= MAX_SIGNING_KEYS,
        "provider driver release trust root must contain one to 32 keys",
    )?;
    let role = &root.provider_driver_release;
    require(
        !role.key_ids.is_empty()
            && usize::try_from(role.threshold)
                .is_ok_and(|threshold| threshold >= 1 && threshold <= role.key_ids.len())
            && (1..=31_536_000).contains(&role.max_authorization_lifetime_seconds),
        "provider driver release signing role threshold or lifetime is invalid",
    )?;
    let mut previous_role_key: Option<&str> = None;
    for key_id in &role.key_ids {
        require(
            is_sha256_hex(key_id)
                && previous_role_key.is_none_or(|previous| previous < key_id.as_str()),
            "provider driver release signing role key IDs must be unique and ordered",
        )?;
        previous_role_key = Some(key_id);
    }
    let mut previous_key: Option<&str> = None;
    for key in &root.keys {
        require(
            is_sha256_hex(&key.key_id)
                && previous_key.is_none_or(|previous| previous < key.key_id.as_str()),
            "provider driver release trust keys must have unique canonical IDs in order",
        )?;
        previous_key = Some(&key.key_id);
        require(
            key.key_type == "ed25519" && key.scheme == "ed25519",
            "provider driver release trust key algorithm is unsupported",
        )?;
        let public_key = decode_canonical_base64(&key.public_key)?;
        require(
            public_key.len() == 32 && key.key_id == hex::encode(Sha256::digest(&public_key)),
            "provider driver release trust key ID does not match its public key",
        )?;
        require(
            key.valid_from < key.valid_until,
            "provider driver release trust key validity window is invalid",
        )?;
        match key.status {
            DriverReleaseSigningKeyStatus::Active => require(
                key.revoked_at.is_none(),
                "active provider driver release trust key cannot have revokedAt",
            )?,
            DriverReleaseSigningKeyStatus::Revoked => require(
                key.revoked_at.is_some(),
                "revoked provider driver release trust key must have revokedAt",
            )?,
        }
    }
    require(
        role.key_ids
            .iter()
            .all(|key_id| root.keys.iter().any(|key| &key.key_id == key_id)),
        "provider driver release signing role references an unknown key",
    )
}

fn validate_authorization(
    authorization: &DriverReleaseAuthorization,
) -> Result<(), DriverReleaseAuthorizationError> {
    let subject = &authorization.subject;
    require(
        authorization
            .schema
            .as_deref()
            .is_some_and(|schema| !schema.is_empty())
            && authorization.schema_version == 1,
        "provider driver release authorization schema identity is invalid",
    )?;
    require(
        subject.release_sha256.algorithm == "sha256"
            && subject.manifest_sha256.algorithm == "sha256",
        "provider driver release authorization only accepts SHA-256 subjects",
    )?;
    require(
        !subject.backend_id.is_empty()
            && !subject.driver_id.is_empty()
            && !subject.driver_version.is_empty(),
        "provider driver release authorization subject identity is incomplete",
    )?;
    require(
        authorization.issued_at < authorization.expires_at,
        "provider driver release authorization validity window is invalid",
    )
}

fn verify_subject(
    subject: &DriverReleaseAuthorizationSubject,
    release: &ValidatedProviderDriverRelease,
    manifest: &ValidatedProviderDriverManifest,
) -> Result<(), DriverReleaseAuthorizationError> {
    release.bind_to_manifest(manifest).map_err(|_| {
        DriverReleaseAuthorizationError::Invalid(
            "driver release authorization documents do not bind to each other",
        )
    })?;
    let release_digest = release.canonical_sha256().map_err(|_| {
        DriverReleaseAuthorizationError::Invalid(
            "driver release authorization cannot identify its release",
        )
    })?;
    let manifest_digest = manifest.canonical_sha256().map_err(|_| {
        DriverReleaseAuthorizationError::Invalid(
            "driver release authorization cannot identify its manifest",
        )
    })?;
    let release_identity = &release.get().driver;
    require(
        subject.release_sha256.value == release_digest
            && subject.manifest_sha256.value == manifest_digest
            && subject.backend_id == manifest.get().backend.id
            && subject.driver_id == release_identity.id
            && subject.driver_version == release_identity.version,
        "driver release authorization subject differs from the exact release and manifest",
    )
}

fn decode_canonical_base64(value: &str) -> Result<Vec<u8>, DriverReleaseAuthorizationError> {
    let decoded = STANDARD.decode(value).map_err(|_| {
        DriverReleaseAuthorizationError::Invalid(
            "driver release authorization contains invalid base64",
        )
    })?;
    if STANDARD.encode(&decoded) != value {
        return Err(DriverReleaseAuthorizationError::Invalid(
            "driver release authorization base64 is not canonical",
        ));
    }
    Ok(decoded)
}

fn dsse_pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut pae = format!(
        "DSSEv1 {} {} {} ",
        payload_type.len(),
        payload_type,
        payload.len()
    )
    .into_bytes();
    pae.extend_from_slice(payload);
    pae
}

fn sha256(bytes: &[u8]) -> Sha256Digest {
    hex::encode(Sha256::digest(bytes))
        .parse()
        .expect("hex encoding a SHA-256 is a canonical digest")
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn require(condition: bool, message: &'static str) -> Result<(), DriverReleaseAuthorizationError> {
    if condition {
        Ok(())
    } else {
        Err(DriverReleaseAuthorizationError::Invalid(message))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use chrono::TimeDelta;
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;

    const RELEASE_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/v1/fixtures/valid/provider-driver-release.json"
    ));
    const MANIFEST_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/v1/fixtures/valid/provider-driver.json"
    ));

    fn release_and_manifest() -> (
        ValidatedProviderDriverRelease,
        ValidatedProviderDriverManifest,
    ) {
        let release = ValidatedProviderDriverRelease::parse_json(RELEASE_FIXTURE).unwrap();
        let mut manifest: crate::ProviderDriverManifest =
            serde_json::from_slice(MANIFEST_FIXTURE).unwrap();
        manifest.driver.id = release.get().driver.id.clone();
        manifest.driver.version = release.get().driver.version.clone();
        manifest.driver.distribution_digest.value = release.canonical_sha256().unwrap();
        manifest.supported_host_operating_systems = release
            .get()
            .distributions
            .iter()
            .map(|distribution| distribution.host_operating_system.as_str().to_owned())
            .collect();
        (
            release,
            ValidatedProviderDriverManifest::new(manifest).unwrap(),
        )
    }

    fn signed_fixture(
        now: DateTime<Utc>,
    ) -> (
        ValidatedDriverReleaseTrustRoot,
        ValidatedDriverReleaseAuthorization,
        ValidatedProviderDriverRelease,
        ValidatedProviderDriverManifest,
    ) {
        let (release, manifest) = release_and_manifest();
        let signing_keys = [
            SigningKey::from_bytes(&[7; 32]),
            SigningKey::from_bytes(&[9; 32]),
        ];
        let mut keys = signing_keys
            .iter()
            .map(|key| {
                let public = key.verifying_key().to_bytes();
                DriverReleaseSigningKey {
                    key_id: hex::encode(Sha256::digest(public)),
                    key_type: "ed25519".to_owned(),
                    scheme: "ed25519".to_owned(),
                    public_key: STANDARD.encode(public),
                    status: DriverReleaseSigningKeyStatus::Active,
                    valid_from: now - TimeDelta::days(1),
                    valid_until: now + TimeDelta::days(90),
                    revoked_at: None,
                }
            })
            .collect::<Vec<_>>();
        keys.sort_by(|left, right| left.key_id.cmp(&right.key_id));
        let root = ValidatedDriverReleaseTrustRoot::new(DriverReleaseTrustRoot {
            schema: Some("provider-driver-trust-root.schema.json".to_owned()),
            schema_version: 1,
            root_version: 1,
            expires_at: now + TimeDelta::days(30),
            provider_driver_release: DriverReleaseSigningRole {
                key_ids: keys.iter().map(|key| key.key_id.clone()).collect(),
                threshold: 2,
                max_authorization_lifetime_seconds: 7 * 24 * 60 * 60,
            },
            keys,
        })
        .unwrap();
        let authorization = DriverReleaseAuthorization {
            schema: Some("provider-driver-release-authorization.schema.json".to_owned()),
            schema_version: 1,
            subject: DriverReleaseAuthorizationSubject {
                release_sha256: ToolDigest {
                    algorithm: "sha256".to_owned(),
                    value: release.canonical_sha256().unwrap(),
                },
                manifest_sha256: ToolDigest {
                    algorithm: "sha256".to_owned(),
                    value: manifest.canonical_sha256().unwrap(),
                },
                backend_id: manifest.get().backend.id.clone(),
                driver_id: release.get().driver.id.clone(),
                driver_version: release.get().driver.version.clone(),
            },
            issued_at: now - TimeDelta::minutes(1),
            expires_at: now + TimeDelta::days(1),
        };
        let payload = serde_jcs::to_vec(&authorization).unwrap();
        let pae = dsse_pae(DSSE_ENVELOPE_PAYLOAD_TYPE, &payload);
        let key_by_id = signing_keys
            .iter()
            .map(|key| {
                (
                    hex::encode(Sha256::digest(key.verifying_key().to_bytes())),
                    key,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let signatures = root
            .get()
            .provider_driver_release
            .key_ids
            .iter()
            .map(|key_id| DriverReleaseDsseSignature {
                keyid: key_id.clone(),
                sig: STANDARD.encode(key_by_id[key_id].sign(&pae).to_bytes()),
            })
            .collect();
        let envelope = ValidatedDriverReleaseAuthorization::new(DriverReleaseDsseEnvelope {
            payload_type: DSSE_ENVELOPE_PAYLOAD_TYPE.to_owned(),
            payload: STANDARD.encode(payload),
            signatures,
        })
        .unwrap();
        (root, envelope, release, manifest)
    }

    #[test]
    fn threshold_dsse_authorizes_only_the_exact_release_and_manifest() {
        let now = Utc::now();
        let (root, envelope, release, manifest) = signed_fixture(now);
        let verified = envelope.verify(&release, &manifest, &root, now).unwrap();
        assert_eq!(verified.trust_root_version, 1);
        assert_eq!(verified.signer_key_ids.len(), 2);
        assert_eq!(verified.trust_root_sha256, *root.canonical_sha256());

        let mut changed_manifest = manifest.get().clone();
        changed_manifest.backend.product.push_str(" changed");
        let changed_manifest = ValidatedProviderDriverManifest::new(changed_manifest).unwrap();
        assert!(
            envelope
                .verify(&release, &changed_manifest, &root, now)
                .unwrap_err()
                .to_string()
                .contains("exact release and manifest")
        );
    }

    #[test]
    fn expired_revoked_and_below_threshold_authorizations_fail_closed() {
        let now = Utc::now();
        let (root, envelope, release, manifest) = signed_fixture(now);
        assert!(
            envelope
                .verify(&release, &manifest, &root, now + TimeDelta::days(2))
                .is_err()
        );

        let mut revoked = root.get().clone();
        revoked.keys[0].status = DriverReleaseSigningKeyStatus::Revoked;
        revoked.keys[0].revoked_at = Some(now);
        let revoked = ValidatedDriverReleaseTrustRoot::new(revoked).unwrap();
        assert!(envelope.verify(&release, &manifest, &revoked, now).is_err());

        let mut below_threshold = envelope.get().clone();
        below_threshold.signatures.pop();
        let below_threshold = ValidatedDriverReleaseAuthorization::new(below_threshold).unwrap();
        assert!(
            below_threshold
                .verify(&release, &manifest, &root, now)
                .unwrap_err()
                .to_string()
                .contains("threshold")
        );
    }

    #[test]
    fn payload_and_signature_malleability_are_rejected() {
        let now = Utc::now();
        let (_root, envelope, _release, _manifest) = signed_fixture(now);
        let mut noncanonical_payload = envelope.get().clone();
        noncanonical_payload.payload =
            STANDARD.encode(serde_json::to_vec_pretty(envelope.authorization()).unwrap());
        assert!(
            ValidatedDriverReleaseAuthorization::new(noncanonical_payload)
                .unwrap_err()
                .to_string()
                .contains("canonical JSON")
        );

        let mut duplicate = envelope.get().clone();
        duplicate.signatures.push(duplicate.signatures[0].clone());
        assert!(ValidatedDriverReleaseAuthorization::new(duplicate).is_err());
    }

    #[test]
    fn shared_contract_authorization_documents_are_semantically_parseable() {
        let root = fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/v1/fixtures/valid/provider-driver-trust-root.json"
        ))
        .unwrap();
        let root = ValidatedDriverReleaseTrustRoot::parse_json(&root).unwrap();

        let envelope = fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/v1/fixtures/valid/provider-driver-release-envelope.json"
        ))
        .unwrap();
        let envelope = ValidatedDriverReleaseAuthorization::parse_json(&envelope).unwrap();
        let release = ValidatedProviderDriverRelease::parse_json(RELEASE_FIXTURE).unwrap();
        let manifest = ValidatedProviderDriverManifest::parse_json(MANIFEST_FIXTURE).unwrap();
        let observed_at = DateTime::parse_from_rfc3339("2026-09-07T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let verified = envelope
            .verify(&release, &manifest, &root, observed_at)
            .unwrap();
        assert_eq!(verified.trust_root_version, 1);
        assert_eq!(verified.signer_key_ids.len(), 2);
    }

    #[test]
    fn trust_root_loader_refuses_relative_or_replaceable_files() {
        let now = Utc::now();
        let (root, _envelope, _release, _manifest) = signed_fixture(now);
        assert!(ValidatedDriverReleaseTrustRoot::load_file("relative.json").is_err());

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("trust-root.json");
        std::fs::write(&path, serde_jcs::to_vec(root.get()).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o600);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        let loaded = ValidatedDriverReleaseTrustRoot::load_file(&path).unwrap();
        assert_eq!(loaded.canonical_sha256(), root.canonical_sha256());

        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt as _, symlink};

            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o666);
            std::fs::set_permissions(&path, permissions).unwrap();
            assert!(ValidatedDriverReleaseTrustRoot::load_file(&path).is_err());

            let link = temporary.path().join("trust-root-link.json");
            symlink(&path, &link).unwrap();
            assert!(ValidatedDriverReleaseTrustRoot::load_file(&link).is_err());
        }
    }
}
