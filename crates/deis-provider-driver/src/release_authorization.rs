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
pub const DSSE_TRUST_ROOT_PAYLOAD_TYPE: &str =
    "application/vnd.deislet.provider-driver-trust-root.v1+json";

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
pub struct DriverReleaseRootRole {
    pub key_ids: Vec<String>,
    pub threshold: u32,
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
    pub root: DriverReleaseRootRole,
    pub provider_driver_release: DriverReleaseSigningRole,
    pub keys: Vec<DriverReleaseSigningKey>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct LegacyDriverReleaseTrustRoot {
    #[serde(rename = "$schema")]
    schema: Option<String>,
    schema_version: u32,
    root_version: u64,
    expires_at: DateTime<Utc>,
    provider_driver_release: DriverReleaseSigningRole,
    keys: Vec<DriverReleaseSigningKey>,
}

#[derive(Clone, Debug)]
pub struct ValidatedDriverReleaseTrustRoot {
    root: DriverReleaseTrustRoot,
    verifying_keys: BTreeMap<String, VerifyingKey>,
    canonical_json: Vec<u8>,
    canonical_sha256: Sha256Digest,
    legacy_implicit_root_role: bool,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverTrustRootDsseEnvelope {
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

#[derive(Clone, Debug)]
pub struct ValidatedDriverTrustRootRotation {
    envelope: DriverTrustRootDsseEnvelope,
    next_root: ValidatedDriverReleaseTrustRoot,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDriverTrustRootRotation {
    pub previous_root_version: u64,
    pub next_root_version: u64,
    pub previous_root_sha256: Sha256Digest,
    pub next_root_sha256: Sha256Digest,
    pub envelope_sha256: Sha256Digest,
    pub previous_root_signer_key_ids: Vec<String>,
    pub next_root_signer_key_ids: Vec<String>,
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
                "cannot read provider driver release trust metadata: {error}"
            ),
            Self::Json(error) => write!(
                formatter,
                "invalid provider driver release trust metadata JSON: {error}"
            ),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DriverReleaseAuthorizationError {}

impl ValidatedDriverReleaseTrustRoot {
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self, DriverReleaseAuthorizationError> {
        let bytes = read_secure_trust_metadata_file(path.as_ref())?;
        Self::parse_json(&bytes)
    }

    pub fn parse_json(bytes: &[u8]) -> Result<Self, DriverReleaseAuthorizationError> {
        let root = serde_json::from_slice(bytes).map_err(DriverReleaseAuthorizationError::Json)?;
        Self::new(root)
    }

    /// Reads the pre-root-role v1 shape only for an already deployed trust
    /// history. New bootstrap and every rotation payload remain strict.
    pub fn load_file_compatible(
        path: impl AsRef<Path>,
    ) -> Result<Self, DriverReleaseAuthorizationError> {
        let bytes = read_secure_trust_metadata_file(path.as_ref())?;
        Self::parse_json_compatible(&bytes)
    }

    pub fn parse_json_compatible(bytes: &[u8]) -> Result<Self, DriverReleaseAuthorizationError> {
        if let Ok(root) = serde_json::from_slice::<DriverReleaseTrustRoot>(bytes) {
            return Self::new(root);
        }
        let legacy: LegacyDriverReleaseTrustRoot =
            serde_json::from_slice(bytes).map_err(DriverReleaseAuthorizationError::Json)?;
        let canonical_json =
            serde_jcs::to_vec(&legacy).map_err(DriverReleaseAuthorizationError::Json)?;
        let root_role = DriverReleaseRootRole {
            key_ids: legacy.provider_driver_release.key_ids.clone(),
            threshold: legacy.provider_driver_release.threshold,
        };
        Self::from_root_and_canonical(
            DriverReleaseTrustRoot {
                schema: legacy.schema,
                schema_version: legacy.schema_version,
                root_version: legacy.root_version,
                expires_at: legacy.expires_at,
                root: root_role,
                provider_driver_release: legacy.provider_driver_release,
                keys: legacy.keys,
            },
            canonical_json,
            true,
        )
    }

    pub fn new(root: DriverReleaseTrustRoot) -> Result<Self, DriverReleaseAuthorizationError> {
        let canonical_json =
            serde_jcs::to_vec(&root).map_err(DriverReleaseAuthorizationError::Json)?;
        Self::from_root_and_canonical(root, canonical_json, false)
    }

    fn from_root_and_canonical(
        root: DriverReleaseTrustRoot,
        canonical_json: Vec<u8>,
        legacy_implicit_root_role: bool,
    ) -> Result<Self, DriverReleaseAuthorizationError> {
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
        Ok(Self {
            root,
            verifying_keys,
            canonical_sha256: sha256(&canonical_json),
            canonical_json,
            legacy_implicit_root_role,
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
        Ok(self.canonical_json.clone())
    }

    #[must_use]
    pub const fn uses_legacy_implicit_root_role(&self) -> bool {
        self.legacy_implicit_root_role
    }

    #[must_use]
    pub fn root_role_satisfies_threshold_at(&self, at: DateTime<Utc>) -> bool {
        self.root
            .root
            .key_ids
            .iter()
            .filter(|key_id| root_key_is_active(self, key_id, at))
            .count()
            >= usize::try_from(self.root.root.threshold).unwrap_or(usize::MAX)
    }
}

impl ValidatedDriverTrustRootRotation {
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self, DriverReleaseAuthorizationError> {
        let bytes = read_secure_trust_metadata_file(path.as_ref())?;
        Self::parse_json(&bytes)
    }

    pub fn parse_json(bytes: &[u8]) -> Result<Self, DriverReleaseAuthorizationError> {
        let envelope =
            serde_json::from_slice(bytes).map_err(DriverReleaseAuthorizationError::Json)?;
        Self::new(envelope)
    }

    pub fn new(
        envelope: DriverTrustRootDsseEnvelope,
    ) -> Result<Self, DriverReleaseAuthorizationError> {
        if envelope.payload_type != DSSE_TRUST_ROOT_PAYLOAD_TYPE {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "driver trust root rotation uses an unsupported DSSE payload type",
            ));
        }
        validate_dsse_signatures(&envelope.signatures)?;
        let payload = decode_canonical_base64(&envelope.payload)?;
        let next_root = ValidatedDriverReleaseTrustRoot::parse_json(&payload)?;
        if payload != next_root.canonical_json()? {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "driver trust root rotation payload must be RFC 8785 canonical JSON",
            ));
        }
        Ok(Self {
            envelope,
            next_root,
            payload,
        })
    }

    #[must_use]
    pub fn get(&self) -> &DriverTrustRootDsseEnvelope {
        &self.envelope
    }

    #[must_use]
    pub fn next_root(&self) -> &ValidatedDriverReleaseTrustRoot {
        &self.next_root
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, DriverReleaseAuthorizationError> {
        serde_jcs::to_vec(&self.envelope).map_err(DriverReleaseAuthorizationError::Json)
    }

    pub fn verify(
        &self,
        previous_root: &ValidatedDriverReleaseTrustRoot,
        now: DateTime<Utc>,
    ) -> Result<VerifiedDriverTrustRootRotation, DriverReleaseAuthorizationError> {
        if self.next_root.root.root_version
            != previous_root.root.root_version.checked_add(1).ok_or(
                DriverReleaseAuthorizationError::Invalid(
                    "provider driver trust root version cannot advance",
                ),
            )?
        {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "provider driver trust root rotation must advance exactly one version",
            ));
        }
        if self.next_root.root.expires_at <= now {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "new provider driver trust root is expired",
            ));
        }

        let pae = dsse_pae(&self.envelope.payload_type, &self.payload);
        let previous_role = &previous_root.root.root;
        let next_role = &self.next_root.root.root;
        let mut previous_signers = Vec::new();
        let mut next_signers = Vec::new();
        for dsse_signature in &self.envelope.signatures {
            let belongs_to_previous = previous_role
                .key_ids
                .iter()
                .any(|key_id| key_id == &dsse_signature.keyid);
            let belongs_to_next = next_role
                .key_ids
                .iter()
                .any(|key_id| key_id == &dsse_signature.keyid);
            if !belongs_to_previous && !belongs_to_next {
                return Err(DriverReleaseAuthorizationError::Invalid(
                    "driver trust root rotation contains a signer outside both root roles",
                ));
            }
            let signature_bytes = decode_canonical_base64(&dsse_signature.sig)?;
            let signature = Signature::from_slice(&signature_bytes).map_err(|_| {
                DriverReleaseAuthorizationError::Invalid(
                    "driver trust root rotation Ed25519 signature is invalid",
                )
            })?;
            let verifying_key = previous_root
                .verifying_keys
                .get(&dsse_signature.keyid)
                .or_else(|| self.next_root.verifying_keys.get(&dsse_signature.keyid))
                .ok_or(DriverReleaseAuthorizationError::Invalid(
                    "driver trust root rotation signer key is absent",
                ))?;
            verifying_key.verify(&pae, &signature).map_err(|_| {
                DriverReleaseAuthorizationError::Invalid(
                    "driver trust root rotation signature verification failed",
                )
            })?;
            if belongs_to_previous && root_key_is_active(previous_root, &dsse_signature.keyid, now)
            {
                previous_signers.push(dsse_signature.keyid.clone());
            }
            if belongs_to_next && root_key_is_active(&self.next_root, &dsse_signature.keyid, now) {
                next_signers.push(dsse_signature.keyid.clone());
            }
        }
        if previous_signers.len() < usize::try_from(previous_role.threshold).unwrap_or(usize::MAX)
            || next_signers.len() < usize::try_from(next_role.threshold).unwrap_or(usize::MAX)
        {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "driver trust root rotation does not satisfy both root signature thresholds",
            ));
        }
        let envelope_json = self.canonical_json()?;
        Ok(VerifiedDriverTrustRootRotation {
            previous_root_version: previous_root.root.root_version,
            next_root_version: self.next_root.root.root_version,
            previous_root_sha256: previous_root.canonical_sha256.clone(),
            next_root_sha256: self.next_root.canonical_sha256.clone(),
            envelope_sha256: sha256(&envelope_json),
            previous_root_signer_key_ids: previous_signers,
            next_root_signer_key_ids: next_signers,
        })
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
        validate_dsse_signatures(&envelope.signatures)?;
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
    let root_role = &root.root;
    require(
        !root_role.key_ids.is_empty()
            && usize::try_from(root_role.threshold)
                .is_ok_and(|threshold| threshold >= 1 && threshold <= root_role.key_ids.len()),
        "provider driver root signing role threshold is invalid",
    )?;
    let mut previous_root_role_key: Option<&str> = None;
    for key_id in &root_role.key_ids {
        require(
            is_sha256_hex(key_id)
                && previous_root_role_key.is_none_or(|previous| previous < key_id.as_str()),
            "provider driver root signing role key IDs must be unique and ordered",
        )?;
        previous_root_role_key = Some(key_id);
    }
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
        root_role
            .key_ids
            .iter()
            .all(|key_id| root.keys.iter().any(|key| &key.key_id == key_id)),
        "provider driver root signing role references an unknown key",
    )?;
    require(
        root_role
            .key_ids
            .iter()
            .filter(|key_id| {
                root.keys.iter().any(|key| {
                    &key.key_id == *key_id && key.status == DriverReleaseSigningKeyStatus::Active
                })
            })
            .count()
            >= usize::try_from(root_role.threshold).unwrap_or(usize::MAX),
        "provider driver root signing role lacks enough active keys",
    )?;
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

fn validate_dsse_signatures(
    signatures: &[DriverReleaseDsseSignature],
) -> Result<(), DriverReleaseAuthorizationError> {
    if signatures.is_empty() || signatures.len() > MAX_SIGNATURES {
        return Err(DriverReleaseAuthorizationError::Invalid(
            "provider driver trust DSSE envelope must contain one to 32 signatures",
        ));
    }
    let mut previous_key_id: Option<&str> = None;
    for signature in signatures {
        if !is_sha256_hex(&signature.keyid)
            || previous_key_id.is_some_and(|previous| previous >= signature.keyid.as_str())
        {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "provider driver trust DSSE signatures must have unique canonical key IDs in order",
            ));
        }
        previous_key_id = Some(&signature.keyid);
        if decode_canonical_base64(&signature.sig)?.len() != 64 {
            return Err(DriverReleaseAuthorizationError::Invalid(
                "provider driver trust DSSE Ed25519 signature must be 64 bytes",
            ));
        }
    }
    Ok(())
}

fn root_key_is_active(
    root: &ValidatedDriverReleaseTrustRoot,
    key_id: &str,
    at: DateTime<Utc>,
) -> bool {
    root.root.keys.iter().any(|key| {
        key.key_id == key_id
            && key.status == DriverReleaseSigningKeyStatus::Active
            && key.valid_from <= at
            && at < key.valid_until
    })
}

fn read_secure_trust_metadata_file(
    path: &Path,
) -> Result<Vec<u8>, DriverReleaseAuthorizationError> {
    if !path.is_absolute() {
        return Err(DriverReleaseAuthorizationError::Invalid(
            "provider driver release trust metadata path must be absolute",
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
            "provider driver release trust metadata must be a regular non-symlink file",
        ));
    }
    #[cfg(windows)]
    if metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
    {
        return Err(DriverReleaseAuthorizationError::Invalid(
            "provider driver release trust metadata cannot be a Windows reparse point",
        ));
    }
    if metadata.len() > MAX_TRUST_ROOT_BYTES {
        return Err(DriverReleaseAuthorizationError::Invalid(
            "provider driver release trust metadata exceeds one MiB",
        ));
    }
    #[cfg(unix)]
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        return Err(DriverReleaseAuthorizationError::Invalid(
            "provider driver release trust metadata must be owned by the service identity and not group/other writable",
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
            "provider driver release trust metadata changed while it was read",
        ));
    }
    Ok(bytes)
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
            root: DriverReleaseRootRole {
                key_ids: vec![hex::encode(Sha256::digest(
                    signing_keys[0].verifying_key().to_bytes(),
                ))],
                threshold: 1,
            },
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

    fn signed_root_rotation(
        previous_root: &ValidatedDriverReleaseTrustRoot,
        now: DateTime<Utc>,
    ) -> ValidatedDriverTrustRootRotation {
        let signing_keys = [
            SigningKey::from_bytes(&[7; 32]),
            SigningKey::from_bytes(&[9; 32]),
            SigningKey::from_bytes(&[11; 32]),
        ];
        let key_by_id = signing_keys
            .iter()
            .map(|key| {
                (
                    hex::encode(Sha256::digest(key.verifying_key().to_bytes())),
                    key,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut next = previous_root.get().clone();
        next.root_version += 1;
        next.expires_at = now + TimeDelta::days(60);
        let retired_key_id =
            hex::encode(Sha256::digest(signing_keys[0].verifying_key().to_bytes()));
        next.keys
            .iter_mut()
            .find(|key| key.key_id == retired_key_id)
            .unwrap()
            .status = DriverReleaseSigningKeyStatus::Revoked;
        next.keys
            .iter_mut()
            .find(|key| key.key_id == retired_key_id)
            .unwrap()
            .revoked_at = Some(now);
        let new_public = signing_keys[2].verifying_key().to_bytes();
        let new_key_id = hex::encode(Sha256::digest(new_public));
        next.keys.push(DriverReleaseSigningKey {
            key_id: new_key_id.clone(),
            key_type: "ed25519".to_owned(),
            scheme: "ed25519".to_owned(),
            public_key: STANDARD.encode(new_public),
            status: DriverReleaseSigningKeyStatus::Active,
            valid_from: now - TimeDelta::days(1),
            valid_until: now + TimeDelta::days(90),
            revoked_at: None,
        });
        next.keys
            .sort_by(|left, right| left.key_id.cmp(&right.key_id));
        let retained_key_id =
            hex::encode(Sha256::digest(signing_keys[1].verifying_key().to_bytes()));
        let mut next_role_key_ids = vec![retained_key_id, new_key_id];
        next_role_key_ids.sort();
        next.root = DriverReleaseRootRole {
            key_ids: next_role_key_ids.clone(),
            threshold: 2,
        };
        next.provider_driver_release.key_ids = next_role_key_ids;
        next.provider_driver_release.threshold = 2;
        let next = ValidatedDriverReleaseTrustRoot::new(next).unwrap();
        let payload = next.canonical_json().unwrap();
        let pae = dsse_pae(DSSE_TRUST_ROOT_PAYLOAD_TYPE, &payload);
        let signatures = key_by_id
            .iter()
            .map(|(key_id, key)| DriverReleaseDsseSignature {
                keyid: key_id.clone(),
                sig: STANDARD.encode(key.sign(&pae).to_bytes()),
            })
            .collect();
        ValidatedDriverTrustRootRotation::new(DriverTrustRootDsseEnvelope {
            payload_type: DSSE_TRUST_ROOT_PAYLOAD_TYPE.to_owned(),
            payload: STANDARD.encode(payload),
            signatures,
        })
        .unwrap()
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
        let revoked_key = revoked
            .keys
            .iter_mut()
            .find(|key| !revoked.root.key_ids.contains(&key.key_id))
            .unwrap();
        revoked_key.status = DriverReleaseSigningKeyStatus::Revoked;
        revoked_key.revoked_at = Some(now);
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
    fn root_rotation_requires_old_and_new_thresholds_and_exactly_next_version() {
        let now = Utc::now();
        let (previous, _authorization, _release, _manifest) = signed_fixture(now);
        let rotation = signed_root_rotation(&previous, now);
        let verified = rotation.verify(&previous, now).unwrap();
        assert_eq!(verified.previous_root_version, 1);
        assert_eq!(verified.next_root_version, 2);
        assert_eq!(verified.previous_root_signer_key_ids.len(), 1);
        assert_eq!(verified.next_root_signer_key_ids.len(), 2);

        let mut expired_previous = previous.get().clone();
        expired_previous.expires_at = now - TimeDelta::seconds(1);
        let expired_previous = ValidatedDriverReleaseTrustRoot::new(expired_previous).unwrap();
        rotation.verify(&expired_previous, now).unwrap();

        let mut expired_previous_key = previous.get().clone();
        let root_key_id = expired_previous_key.root.key_ids[0].clone();
        expired_previous_key
            .keys
            .iter_mut()
            .find(|key| key.key_id == root_key_id)
            .unwrap()
            .valid_until = now - TimeDelta::seconds(1);
        let expired_previous_key =
            ValidatedDriverReleaseTrustRoot::new(expired_previous_key).unwrap();
        assert!(
            rotation
                .verify(&expired_previous_key, now)
                .unwrap_err()
                .to_string()
                .contains("both root signature thresholds")
        );

        let previous_only_key = previous
            .get()
            .root
            .key_ids
            .iter()
            .find(|key_id| !rotation.next_root().get().root.key_ids.contains(key_id))
            .unwrap()
            .clone();
        let mut without_previous_threshold = rotation.get().clone();
        without_previous_threshold
            .signatures
            .retain(|signature| signature.keyid != previous_only_key);
        let without_previous_threshold =
            ValidatedDriverTrustRootRotation::new(without_previous_threshold).unwrap();
        assert!(
            without_previous_threshold
                .verify(&previous, now)
                .unwrap_err()
                .to_string()
                .contains("both root signature thresholds")
        );

        let next_only_key = rotation
            .next_root()
            .get()
            .root
            .key_ids
            .iter()
            .find(|key_id| !previous.get().root.key_ids.contains(key_id))
            .unwrap()
            .clone();
        let mut without_next_threshold = rotation.get().clone();
        without_next_threshold
            .signatures
            .retain(|signature| signature.keyid != next_only_key);
        let without_next_threshold =
            ValidatedDriverTrustRootRotation::new(without_next_threshold).unwrap();
        assert!(without_next_threshold.verify(&previous, now).is_err());

        let mut skipped = rotation.next_root().get().clone();
        skipped.root_version += 1;
        let skipped = ValidatedDriverReleaseTrustRoot::new(skipped).unwrap();
        let skipped_payload = skipped.canonical_json().unwrap();
        let mut skipped_envelope = rotation.get().clone();
        skipped_envelope.payload = STANDARD.encode(skipped_payload);
        let skipped = ValidatedDriverTrustRootRotation::new(skipped_envelope).unwrap();
        assert!(
            skipped
                .verify(&previous, now)
                .unwrap_err()
                .to_string()
                .contains("exactly one version")
        );
    }

    #[test]
    fn legacy_root_shape_preserves_its_identity_only_in_compatible_mode() {
        let now = Utc::now();
        let (root, _authorization, _release, _manifest) = signed_fixture(now);
        let mut legacy: serde_json::Value = serde_json::to_value(root.get()).unwrap();
        legacy.as_object_mut().unwrap().remove("root");
        let canonical_legacy = serde_jcs::to_vec(&legacy).unwrap();

        assert!(ValidatedDriverReleaseTrustRoot::parse_json(&canonical_legacy).is_err());
        let compatible =
            ValidatedDriverReleaseTrustRoot::parse_json_compatible(&canonical_legacy).unwrap();
        assert!(compatible.uses_legacy_implicit_root_role());
        assert_eq!(compatible.canonical_json().unwrap(), canonical_legacy);
        assert_eq!(
            compatible.get().root.key_ids,
            compatible.get().provider_driver_release.key_ids
        );
        assert_eq!(
            compatible.get().root.threshold,
            compatible.get().provider_driver_release.threshold
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

        let rotation = fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/v1/fixtures/valid/provider-driver-trust-root-envelope.json"
        ))
        .unwrap();
        let rotation = ValidatedDriverTrustRootRotation::parse_json(&rotation).unwrap();
        let verified_rotation = rotation.verify(&root, observed_at).unwrap();
        assert_eq!(verified_rotation.previous_root_version, 1);
        assert_eq!(verified_rotation.next_root_version, 2);
        assert_eq!(verified_rotation.previous_root_signer_key_ids.len(), 1);
        assert_eq!(verified_rotation.next_root_signer_key_ids.len(), 2);
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
