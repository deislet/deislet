//! Offline-verifiable transparency proofs for provider driver authorizations.
//!
//! The JSON wrapper binds one exact canonical DSSE authorization to one or
//! more C2SP/RFC 6962 inclusion proofs. Checkpoints use the C2SP signed-note
//! format and must carry a threshold of timestamped C2SP witness
//! cosignatures, so verification neither trusts an online log response nor an
//! unauthenticated integration timestamp.

use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    DriverReleaseAuthorizationError, DriverReleaseSigningKey, DriverReleaseSigningKeyStatus,
    Sha256Digest, ToolDigest, ValidatedDriverReleaseAuthorization, ValidatedDriverReleaseTrustRoot,
};

pub const TRANSPARENCY_PROOF_MEDIA_TYPE: &str =
    "application/vnd.deislet.provider-driver-transparency-proof.v1+json";

const MAX_TRANSPARENCY_IDENTITIES: usize = 32;
const MAX_TRANSPARENCY_ENTRIES: usize = 16;
const MAX_TRANSPARENCY_PROOF_BYTES: usize = 2 * 1024 * 1024;
const MAX_INCLUSION_PATH_HASHES: usize = 64;
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
const MAX_NOTE_SIGNATURES: usize = 64;
const NOTE_ED25519_SIGNATURE_TYPE: u8 = 0x01;
const NOTE_TIMESTAMPED_ED25519_COSIGNATURE_TYPE: u8 = 0x04;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseTransparencyLog {
    pub origin: String,
    pub operator: String,
    pub key_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseTransparencyWitness {
    pub name: String,
    pub operator: String,
    pub key_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseTransparencyPolicy {
    pub log_operator_threshold: u32,
    pub witness_operator_threshold: u32,
    pub logs: Vec<DriverReleaseTransparencyLog>,
    pub witnesses: Vec<DriverReleaseTransparencyWitness>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseTransparencyEntry {
    pub log_origin: String,
    pub log_index: u64,
    pub inclusion_path: Vec<String>,
    pub checkpoint: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseTransparencyProof {
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    pub schema_version: u32,
    pub media_type: String,
    pub authorization_sha256: ToolDigest,
    pub entries: Vec<DriverReleaseTransparencyEntry>,
}

#[derive(Clone, Debug)]
pub struct ValidatedDriverReleaseTransparencyProof {
    proof: DriverReleaseTransparencyProof,
    canonical_json: Vec<u8>,
    canonical_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDriverReleaseTransparencyProof {
    pub proof_sha256: Sha256Digest,
    pub authorization_sha256: Sha256Digest,
    pub log_origins: Vec<String>,
    pub log_operators: Vec<String>,
    pub witness_operators: Vec<String>,
    pub earliest_witnessed_at: DateTime<Utc>,
    pub latest_witnessed_at: DateTime<Utc>,
}

#[derive(Debug)]
struct ParsedCheckpoint {
    body: Vec<u8>,
    origin: String,
    tree_size: u64,
    root_hash: [u8; 32],
    signatures: Vec<ParsedNoteSignature>,
}

#[derive(Debug)]
struct ParsedNoteSignature {
    name: String,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct VerifiedCheckpoint {
    log_operator: String,
    witness_operators: BTreeSet<String>,
    witness_times: Vec<DateTime<Utc>>,
}

impl ValidatedDriverReleaseTransparencyProof {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, DriverReleaseAuthorizationError> {
        if bytes.len() > MAX_TRANSPARENCY_PROOF_BYTES {
            return Err(invalid(
                "provider driver transparency proof exceeds its input limit",
            ));
        }
        let proof = serde_json::from_slice(bytes).map_err(DriverReleaseAuthorizationError::Json)?;
        Self::new(proof)
    }

    pub fn new(
        proof: DriverReleaseTransparencyProof,
    ) -> Result<Self, DriverReleaseAuthorizationError> {
        validate_proof_shape(&proof)?;
        let canonical_json =
            serde_jcs::to_vec(&proof).map_err(DriverReleaseAuthorizationError::Json)?;
        let canonical_sha256 = sha256_digest(&canonical_json);
        Ok(Self {
            proof,
            canonical_json,
            canonical_sha256,
        })
    }

    #[must_use]
    pub fn get(&self) -> &DriverReleaseTransparencyProof {
        &self.proof
    }

    #[must_use]
    pub fn canonical_sha256(&self) -> &Sha256Digest {
        &self.canonical_sha256
    }

    #[must_use]
    pub fn canonical_json(&self) -> Vec<u8> {
        self.canonical_json.clone()
    }

    pub fn verify(
        &self,
        authorization: &ValidatedDriverReleaseAuthorization,
        trust_root: &ValidatedDriverReleaseTrustRoot,
        now: DateTime<Utc>,
    ) -> Result<VerifiedDriverReleaseTransparencyProof, DriverReleaseAuthorizationError> {
        let authorization_json = authorization.canonical_json()?;
        let authorization_sha256 = sha256_digest(&authorization_json);
        if self.proof.authorization_sha256.algorithm != "sha256"
            || self.proof.authorization_sha256.value != authorization_sha256
        {
            return Err(invalid(
                "provider driver transparency proof does not bind the canonical authorization envelope",
            ));
        }
        let policy = trust_root
            .get()
            .provider_driver_transparency
            .as_ref()
            .ok_or_else(|| invalid("provider driver trust root has no transparency policy"))?;
        let leaf_hash = rfc6962_leaf_hash(&authorization_json);
        let mut log_operators = BTreeSet::new();
        let mut witness_operators = BTreeSet::new();
        let mut log_origins = Vec::with_capacity(self.proof.entries.len());
        let mut witness_times = Vec::new();
        for entry in &self.proof.entries {
            let checkpoint = parse_checkpoint(&entry.checkpoint)?;
            if checkpoint.origin != entry.log_origin || entry.log_index >= checkpoint.tree_size {
                return Err(invalid(
                    "provider driver transparency entry differs from its checkpoint identity",
                ));
            }
            let expected_root = inclusion_root(
                leaf_hash,
                entry.log_index,
                checkpoint.tree_size,
                &entry.inclusion_path,
            )?;
            if expected_root != checkpoint.root_hash {
                return Err(invalid(
                    "provider driver transparency inclusion proof does not reach the signed checkpoint root",
                ));
            }
            let verified = verify_checkpoint(
                &checkpoint,
                policy,
                &trust_root.get().keys,
                authorization,
                now,
            )?;
            log_origins.push(entry.log_origin.clone());
            log_operators.insert(verified.log_operator);
            witness_operators.extend(verified.witness_operators);
            witness_times.extend(verified.witness_times);
        }
        if log_operators.len()
            < usize::try_from(policy.log_operator_threshold).unwrap_or(usize::MAX)
        {
            return Err(invalid(
                "provider driver transparency proof lacks the required distinct log operators",
            ));
        }
        let earliest_witnessed_at =
            witness_times.iter().min().copied().ok_or_else(|| {
                invalid("provider driver transparency proof has no trusted witness")
            })?;
        let latest_witnessed_at =
            witness_times.iter().max().copied().ok_or_else(|| {
                invalid("provider driver transparency proof has no trusted witness")
            })?;
        Ok(VerifiedDriverReleaseTransparencyProof {
            proof_sha256: self.canonical_sha256.clone(),
            authorization_sha256,
            log_origins,
            log_operators: log_operators.into_iter().collect(),
            witness_operators: witness_operators.into_iter().collect(),
            earliest_witnessed_at,
            latest_witnessed_at,
        })
    }
}

pub(crate) fn validate_transparency_policy(
    policy: &DriverReleaseTransparencyPolicy,
    keys: &[DriverReleaseSigningKey],
) -> Result<(), DriverReleaseAuthorizationError> {
    if policy.logs.is_empty()
        || policy.logs.len() > MAX_TRANSPARENCY_IDENTITIES
        || policy.witnesses.is_empty()
        || policy.witnesses.len() > MAX_TRANSPARENCY_IDENTITIES
    {
        return Err(invalid(
            "provider driver transparency policy must contain one to 32 logs and witnesses",
        ));
    }
    let key_by_id = keys
        .iter()
        .map(|key| (key.key_id.as_str(), key))
        .collect::<BTreeMap<_, _>>();
    let mut prior_log: Option<(&str, &str, &str)> = None;
    let mut log_operators = BTreeSet::new();
    let mut log_origins = BTreeSet::new();
    let mut transparency_key_ids = BTreeSet::new();
    for log in &policy.logs {
        validate_identity_name(&log.origin)?;
        validate_identity_name(&log.operator)?;
        let identity = (
            log.operator.as_str(),
            log.origin.as_str(),
            log.key_id.as_str(),
        );
        if prior_log.is_some_and(|prior| prior >= identity) {
            return Err(invalid(
                "provider driver transparency logs must be unique and canonically ordered",
            ));
        }
        if !log_origins.insert(log.origin.as_str()) {
            return Err(invalid(
                "provider driver transparency log origins must be unique",
            ));
        }
        require_active_policy_key(&key_by_id, &log.key_id)?;
        if !transparency_key_ids.insert(log.key_id.as_str()) {
            return Err(invalid(
                "provider driver transparency identities cannot reuse a key",
            ));
        }
        prior_log = Some(identity);
        log_operators.insert(log.operator.as_str());
    }
    let mut prior_witness: Option<(&str, &str, &str)> = None;
    let mut witness_operators = BTreeSet::new();
    let mut witness_names = BTreeSet::new();
    for witness in &policy.witnesses {
        validate_identity_name(&witness.name)?;
        validate_identity_name(&witness.operator)?;
        let identity = (
            witness.operator.as_str(),
            witness.name.as_str(),
            witness.key_id.as_str(),
        );
        if prior_witness.is_some_and(|prior| prior >= identity) {
            return Err(invalid(
                "provider driver transparency witnesses must be unique and canonically ordered",
            ));
        }
        if !witness_names.insert(witness.name.as_str()) {
            return Err(invalid(
                "provider driver transparency witness names must be unique",
            ));
        }
        require_active_policy_key(&key_by_id, &witness.key_id)?;
        if !transparency_key_ids.insert(witness.key_id.as_str()) {
            return Err(invalid(
                "provider driver transparency identities cannot reuse a key",
            ));
        }
        prior_witness = Some(identity);
        witness_operators.insert(witness.operator.as_str());
    }
    if usize::try_from(policy.log_operator_threshold).map_or(true, |threshold| {
        threshold == 0 || threshold > log_operators.len() || threshold > MAX_TRANSPARENCY_ENTRIES
    }) || usize::try_from(policy.witness_operator_threshold).map_or(true, |threshold| {
        threshold == 0 || threshold > witness_operators.len()
    }) {
        return Err(invalid(
            "provider driver transparency operator threshold is invalid",
        ));
    }
    if !log_operators.is_disjoint(&witness_operators) {
        return Err(invalid(
            "provider driver transparency logs and witnesses require independent operators",
        ));
    }
    Ok(())
}

fn validate_proof_shape(
    proof: &DriverReleaseTransparencyProof,
) -> Result<(), DriverReleaseAuthorizationError> {
    if proof
        .schema
        .as_deref()
        .is_none_or(|schema| schema.is_empty() || schema.len() > 256)
        || proof.schema_version != 1
        || proof.media_type != TRANSPARENCY_PROOF_MEDIA_TYPE
        || proof.authorization_sha256.algorithm != "sha256"
    {
        return Err(invalid(
            "provider driver transparency proof schema or media identity is invalid",
        ));
    }
    if proof.entries.is_empty() || proof.entries.len() > MAX_TRANSPARENCY_ENTRIES {
        return Err(invalid(
            "provider driver transparency proof must contain one to 16 entries",
        ));
    }
    let mut prior_origin: Option<&str> = None;
    for entry in &proof.entries {
        validate_identity_name(&entry.log_origin)?;
        if prior_origin.is_some_and(|prior| prior >= entry.log_origin.as_str()) {
            return Err(invalid(
                "provider driver transparency entries must have unique ordered log origins",
            ));
        }
        if entry.inclusion_path.len() > MAX_INCLUSION_PATH_HASHES
            || entry.checkpoint.len() > MAX_CHECKPOINT_BYTES
        {
            return Err(invalid(
                "provider driver transparency entry exceeds its proof limits",
            ));
        }
        for hash in &entry.inclusion_path {
            if decode_canonical_base64(hash)?.len() != 32 {
                return Err(invalid(
                    "provider driver transparency inclusion path hashes must be SHA-256 width",
                ));
            }
        }
        prior_origin = Some(&entry.log_origin);
    }
    Ok(())
}

fn parse_checkpoint(value: &str) -> Result<ParsedCheckpoint, DriverReleaseAuthorizationError> {
    if value.is_empty() || value.len() > MAX_CHECKPOINT_BYTES || !value.ends_with('\n') {
        return Err(invalid(
            "provider driver transparency checkpoint is not a bounded newline-terminated signed note",
        ));
    }
    if value
        .chars()
        .any(|character| character < ' ' && character != '\n')
    {
        return Err(invalid(
            "provider driver transparency checkpoint contains an unsupported control character",
        ));
    }
    let separator = value.rfind("\n\n").ok_or_else(|| {
        invalid("provider driver transparency checkpoint has no signed-note separator")
    })?;
    let body = &value[..separator + 1];
    let signature_text = &value[separator + 2..];
    if body.contains("\n\n") || signature_text.is_empty() {
        return Err(invalid(
            "provider driver transparency checkpoint body or signature section is invalid",
        ));
    }
    let body_without_newline = body
        .strip_suffix('\n')
        .expect("body includes signed-note separator newline");
    let lines = body_without_newline.split('\n').collect::<Vec<_>>();
    if lines.len() != 3 {
        return Err(invalid(
            "provider driver transparency checkpoint extensions are not supported",
        ));
    }
    let origin = lines[0];
    validate_identity_name(origin)?;
    let tree_size = parse_canonical_u64(lines[1])?;
    if tree_size == 0 {
        return Err(invalid(
            "provider driver transparency checkpoint tree cannot be empty",
        ));
    }
    let root_hash: [u8; 32] = decode_canonical_base64(lines[2])?.try_into().map_err(|_| {
        invalid("provider driver transparency checkpoint root must be SHA-256 width")
    })?;
    let signature_lines = signature_text
        .strip_suffix('\n')
        .ok_or_else(|| invalid("provider driver transparency signature note is not terminated"))?
        .split('\n')
        .collect::<Vec<_>>();
    if signature_lines.is_empty() || signature_lines.len() > MAX_NOTE_SIGNATURES {
        return Err(invalid(
            "provider driver transparency checkpoint has an invalid signature count",
        ));
    }
    let mut signatures = Vec::with_capacity(signature_lines.len());
    for line in signature_lines {
        let rest = line.strip_prefix("— ").ok_or_else(|| {
            invalid("provider driver transparency checkpoint signature line is invalid")
        })?;
        let (name, encoded) = rest.split_once(' ').ok_or_else(|| {
            invalid("provider driver transparency checkpoint signature line is incomplete")
        })?;
        validate_identity_name(name)?;
        let bytes = decode_canonical_base64(encoded)?;
        if bytes.len() < 4 || bytes.len() > 5_000 {
            return Err(invalid(
                "provider driver transparency checkpoint signature width is invalid",
            ));
        }
        signatures.push(ParsedNoteSignature {
            name: name.to_owned(),
            bytes,
        });
    }
    Ok(ParsedCheckpoint {
        body: body.as_bytes().to_vec(),
        origin: origin.to_owned(),
        tree_size,
        root_hash,
        signatures,
    })
}

fn verify_checkpoint(
    checkpoint: &ParsedCheckpoint,
    policy: &DriverReleaseTransparencyPolicy,
    keys: &[DriverReleaseSigningKey],
    authorization: &ValidatedDriverReleaseAuthorization,
    now: DateTime<Utc>,
) -> Result<VerifiedCheckpoint, DriverReleaseAuthorizationError> {
    let keys_by_id = keys
        .iter()
        .map(|key| (key.key_id.as_str(), key))
        .collect::<BTreeMap<_, _>>();
    let mut verified_log_keys = Vec::new();
    let mut seen_known_signatures = BTreeSet::new();
    for log in policy
        .logs
        .iter()
        .filter(|log| log.origin == checkpoint.origin)
    {
        let key = keys_by_id
            .get(log.key_id.as_str())
            .ok_or_else(|| invalid("provider driver transparency log key is absent"))?;
        let public_key = verifying_key(key)?;
        let expected_id = note_key_id(
            &log.origin,
            NOTE_ED25519_SIGNATURE_TYPE,
            public_key.as_bytes(),
        );
        for note_signature in checkpoint.signatures.iter().filter(|signature| {
            signature.name == log.origin && signature.bytes.starts_with(&expected_id)
        }) {
            if !seen_known_signatures.insert((note_signature.name.clone(), expected_id)) {
                return Err(invalid(
                    "provider driver transparency checkpoint repeats a trusted signature",
                ));
            }
            if note_signature.bytes.len() != 68 {
                return Err(invalid(
                    "provider driver transparency log signature must be Ed25519 signed-note width",
                ));
            }
            let signature = Signature::from_slice(&note_signature.bytes[4..])
                .map_err(|_| invalid("provider driver transparency log signature is malformed"))?;
            public_key
                .verify(&checkpoint.body, &signature)
                .map_err(|_| invalid("provider driver transparency log signature is invalid"))?;
            verified_log_keys.push(log);
        }
    }
    if verified_log_keys.is_empty() {
        return Err(invalid(
            "provider driver transparency checkpoint has no trusted log signature",
        ));
    }

    let mut witness_operators = BTreeSet::new();
    let mut witness_times = Vec::new();
    for witness in &policy.witnesses {
        let key = keys_by_id
            .get(witness.key_id.as_str())
            .ok_or_else(|| invalid("provider driver transparency witness key is absent"))?;
        let public_key = verifying_key(key)?;
        let expected_id = note_key_id(
            &witness.name,
            NOTE_TIMESTAMPED_ED25519_COSIGNATURE_TYPE,
            public_key.as_bytes(),
        );
        for note_signature in checkpoint.signatures.iter().filter(|signature| {
            signature.name == witness.name && signature.bytes.starts_with(&expected_id)
        }) {
            if !seen_known_signatures.insert((note_signature.name.clone(), expected_id)) {
                return Err(invalid(
                    "provider driver transparency checkpoint repeats a trusted signature",
                ));
            }
            if note_signature.bytes.len() != 76 {
                return Err(invalid(
                    "provider driver transparency witness signature must be timestamped Ed25519 width",
                ));
            }
            let timestamp_seconds = u64::from_be_bytes(
                note_signature.bytes[4..12]
                    .try_into()
                    .expect("checked timestamped signature width"),
            );
            let timestamp_seconds = i64::try_from(timestamp_seconds).map_err(|_| {
                invalid("provider driver transparency witness timestamp exceeds POSIX range")
            })?;
            let witnessed_at = DateTime::from_timestamp(timestamp_seconds, 0).ok_or_else(|| {
                invalid("provider driver transparency witness timestamp is invalid")
            })?;
            if witnessed_at < authorization.authorization().issued_at
                || witnessed_at >= authorization.authorization().expires_at
                || witnessed_at > now
                || !key_is_active_at(key, witnessed_at)
            {
                return Err(invalid(
                    "provider driver transparency witness time is outside the trusted authorization and key windows",
                ));
            }
            let checkpoint_body = std::str::from_utf8(&checkpoint.body)
                .expect("signed checkpoint input was parsed from a UTF-8 string");
            let signed_message =
                format!("cosignature/v1\ntime {timestamp_seconds}\n{checkpoint_body}");
            let signature = Signature::from_slice(&note_signature.bytes[12..]).map_err(|_| {
                invalid("provider driver transparency witness signature is malformed")
            })?;
            public_key
                .verify(signed_message.as_bytes(), &signature)
                .map_err(|_| {
                    invalid("provider driver transparency witness signature is invalid")
                })?;
            witness_operators.insert(witness.operator.clone());
            witness_times.push(witnessed_at);
        }
    }
    if witness_operators.len()
        < usize::try_from(policy.witness_operator_threshold).unwrap_or(usize::MAX)
    {
        return Err(invalid(
            "provider driver transparency checkpoint lacks the required distinct witness operators",
        ));
    }
    let valid_log = verified_log_keys.into_iter().find(|log| {
        keys_by_id.get(log.key_id.as_str()).is_some_and(|key| {
            witness_times
                .iter()
                .copied()
                .any(|witnessed_at| key_is_active_at(key, witnessed_at))
        })
    });
    let log = valid_log.ok_or_else(|| {
        invalid("provider driver transparency log key was not valid when witnesses observed it")
    })?;
    Ok(VerifiedCheckpoint {
        log_operator: log.operator.clone(),
        witness_operators,
        witness_times,
    })
}

fn inclusion_root(
    leaf_hash: [u8; 32],
    leaf_index: u64,
    tree_size: u64,
    proof: &[String],
) -> Result<[u8; 32], DriverReleaseAuthorizationError> {
    if tree_size == 0 || leaf_index >= tree_size {
        return Err(invalid(
            "provider driver transparency inclusion index is outside the tree",
        ));
    }
    let mut node_index = leaf_index;
    let mut last_node = tree_size - 1;
    let mut root = leaf_hash;
    for encoded_hash in proof {
        if last_node == 0 {
            return Err(invalid(
                "provider driver transparency inclusion path is too long",
            ));
        }
        let sibling: [u8; 32] = decode_canonical_base64(encoded_hash)?
            .try_into()
            .map_err(|_| invalid("provider driver transparency inclusion path hash is invalid"))?;
        if node_index & 1 == 1 || node_index == last_node {
            root = rfc6962_node_hash(&sibling, &root);
            while node_index != 0 && node_index & 1 == 0 {
                node_index >>= 1;
                last_node >>= 1;
            }
        } else {
            root = rfc6962_node_hash(&root, &sibling);
        }
        node_index >>= 1;
        last_node >>= 1;
    }
    if last_node != 0 {
        return Err(invalid(
            "provider driver transparency inclusion path is incomplete",
        ));
    }
    Ok(root)
}

fn require_active_policy_key<'a>(
    keys: &BTreeMap<&str, &'a DriverReleaseSigningKey>,
    key_id: &str,
) -> Result<&'a DriverReleaseSigningKey, DriverReleaseAuthorizationError> {
    let key = keys.get(key_id).copied().ok_or_else(|| {
        invalid("provider driver transparency identity references an unknown key")
    })?;
    if key.status != DriverReleaseSigningKeyStatus::Active || key.revoked_at.is_some() {
        return Err(invalid(
            "provider driver transparency identity requires an active key",
        ));
    }
    Ok(key)
}

fn verifying_key(
    key: &DriverReleaseSigningKey,
) -> Result<VerifyingKey, DriverReleaseAuthorizationError> {
    let bytes: [u8; 32] = decode_canonical_base64(&key.public_key)?
        .try_into()
        .map_err(|_| invalid("provider driver transparency Ed25519 key must be 32 bytes"))?;
    VerifyingKey::from_bytes(&bytes)
        .map_err(|_| invalid("provider driver transparency Ed25519 key is invalid"))
}

fn key_is_active_at(key: &DriverReleaseSigningKey, at: DateTime<Utc>) -> bool {
    key.status == DriverReleaseSigningKeyStatus::Active
        && key.revoked_at.is_none()
        && key.valid_from <= at
        && at < key.valid_until
}

fn note_key_id(name: &str, signature_type: u8, public_key: &[u8; 32]) -> [u8; 4] {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update(b"\n");
    hasher.update([signature_type]);
    hasher.update(public_key);
    hasher.finalize()[..4]
        .try_into()
        .expect("SHA-256 has at least four bytes")
}

fn rfc6962_leaf_hash(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([0]);
    hasher.update(bytes);
    hasher.finalize().into()
}

fn rfc6962_node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([1]);
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

fn parse_canonical_u64(value: &str) -> Result<u64, DriverReleaseAuthorizationError> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid(
            "provider driver transparency checkpoint tree size is not canonical decimal",
        ));
    }
    value
        .parse()
        .map_err(|_| invalid("provider driver transparency checkpoint tree size exceeds its range"))
}

fn validate_identity_name(value: &str) -> Result<(), DriverReleaseAuthorizationError> {
    if value.is_empty()
        || value.len() > 256
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte <= b' ' || byte == b'+' || byte == 0x7f)
    {
        return Err(invalid(
            "provider driver transparency identity must be bounded ASCII without spaces or plus",
        ));
    }
    Ok(())
}

fn decode_canonical_base64(value: &str) -> Result<Vec<u8>, DriverReleaseAuthorizationError> {
    let decoded = STANDARD
        .decode(value)
        .map_err(|_| invalid("provider driver transparency proof contains invalid base64"))?;
    if STANDARD.encode(&decoded) != value {
        return Err(invalid(
            "provider driver transparency proof base64 is not canonical",
        ));
    }
    Ok(decoded)
}

fn sha256_digest(bytes: &[u8]) -> Sha256Digest {
    hex::encode(Sha256::digest(bytes))
        .parse()
        .expect("hex encoding a SHA-256 is a canonical digest")
}

const fn invalid(message: &'static str) -> DriverReleaseAuthorizationError {
    DriverReleaseAuthorizationError::Invalid(message)
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;
    use crate::{
        DSSE_ENVELOPE_PAYLOAD_TYPE, DriverReleaseAuthorization, DriverReleaseAuthorizationSubject,
        DriverReleaseDsseEnvelope, DriverReleaseDsseSignature, DriverReleaseRootRole,
        DriverReleaseSigningRole, DriverReleaseTrustRoot,
    };

    fn reference_tree_split(length: usize) -> usize {
        if length.is_power_of_two() {
            length / 2
        } else {
            1 << length.ilog2()
        }
    }

    fn reference_merkle_root(leaves: &[[u8; 32]]) -> [u8; 32] {
        assert!(!leaves.is_empty());
        if leaves.len() == 1 {
            return leaves[0];
        }
        let split = reference_tree_split(leaves.len());
        rfc6962_node_hash(
            &reference_merkle_root(&leaves[..split]),
            &reference_merkle_root(&leaves[split..]),
        )
    }

    fn reference_inclusion_path(leaves: &[[u8; 32]], index: usize, path: &mut Vec<String>) {
        if leaves.len() == 1 {
            return;
        }
        let split = reference_tree_split(leaves.len());
        if index < split {
            reference_inclusion_path(&leaves[..split], index, path);
            path.push(STANDARD.encode(reference_merkle_root(&leaves[split..])));
        } else {
            reference_inclusion_path(&leaves[split..], index - split, path);
            path.push(STANDARD.encode(reference_merkle_root(&leaves[..split])));
        }
    }

    fn key(seed: u8, now: DateTime<Utc>) -> (SigningKey, DriverReleaseSigningKey) {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let public = signing.verifying_key().to_bytes();
        let trusted = DriverReleaseSigningKey {
            key_id: hex::encode(Sha256::digest(public)),
            key_type: "ed25519".to_owned(),
            scheme: "ed25519".to_owned(),
            public_key: STANDARD.encode(public),
            status: DriverReleaseSigningKeyStatus::Active,
            valid_from: now - TimeDelta::days(1),
            valid_until: now + TimeDelta::days(30),
            revoked_at: None,
        };
        (signing, trusted)
    }

    fn authorization(now: DateTime<Utc>) -> ValidatedDriverReleaseAuthorization {
        let payload = serde_jcs::to_vec(&DriverReleaseAuthorization {
            schema: Some("provider-driver-release-authorization.schema.json".to_owned()),
            schema_version: 1,
            subject: DriverReleaseAuthorizationSubject {
                release_sha256: ToolDigest {
                    algorithm: "sha256".to_owned(),
                    value: "1".repeat(64).parse().unwrap(),
                },
                manifest_sha256: ToolDigest {
                    algorithm: "sha256".to_owned(),
                    value: "2".repeat(64).parse().unwrap(),
                },
                backend_id: "deislet".to_owned(),
                driver_id: "deislet.driver".to_owned(),
                driver_version: "1.0.0".to_owned(),
            },
            issued_at: now - TimeDelta::minutes(2),
            expires_at: now + TimeDelta::days(1),
        })
        .unwrap();
        ValidatedDriverReleaseAuthorization::new(DriverReleaseDsseEnvelope {
            payload_type: DSSE_ENVELOPE_PAYLOAD_TYPE.to_owned(),
            payload: STANDARD.encode(payload),
            signatures: vec![DriverReleaseDsseSignature {
                keyid: "0".repeat(64),
                sig: STANDARD.encode([0_u8; 64]),
            }],
        })
        .unwrap()
    }

    fn fixture(
        now: DateTime<Utc>,
    ) -> (
        ValidatedDriverReleaseTrustRoot,
        ValidatedDriverReleaseAuthorization,
        ValidatedDriverReleaseTransparencyProof,
    ) {
        let (release_signing, release_key) = key(1, now);
        let (log_signing, log_key) = key(2, now);
        let (witness_signing, witness_key) = key(3, now);
        let log_origin = "log.example.test/deislet";
        let witness_name = "witness.example.test/deislet";
        let mut keys = vec![release_key.clone(), log_key.clone(), witness_key.clone()];
        keys.sort_by(|left, right| left.key_id.cmp(&right.key_id));
        let root = ValidatedDriverReleaseTrustRoot::new(DriverReleaseTrustRoot {
            schema: Some("provider-driver-trust-root.schema.json".to_owned()),
            schema_version: 1,
            root_version: 1,
            expires_at: now + TimeDelta::days(20),
            root: DriverReleaseRootRole {
                key_ids: vec![release_key.key_id.clone()],
                threshold: 1,
            },
            provider_driver_release: DriverReleaseSigningRole {
                key_ids: vec![release_key.key_id],
                threshold: 1,
                max_authorization_lifetime_seconds: 86_400,
            },
            provider_driver_transparency: Some(DriverReleaseTransparencyPolicy {
                log_operator_threshold: 1,
                witness_operator_threshold: 1,
                logs: vec![DriverReleaseTransparencyLog {
                    origin: log_origin.to_owned(),
                    operator: "log-operator.example.test".to_owned(),
                    key_id: log_key.key_id,
                }],
                witnesses: vec![DriverReleaseTransparencyWitness {
                    name: witness_name.to_owned(),
                    operator: "witness-operator.example.test".to_owned(),
                    key_id: witness_key.key_id,
                }],
            }),
            keys,
        })
        .unwrap();
        drop(release_signing);
        let authorization = authorization(now);
        let authorization_json = authorization.canonical_json().unwrap();
        let authorization_sha256 = sha256_digest(&authorization_json);
        let root_hash = rfc6962_leaf_hash(&authorization_json);
        let body = format!("{log_origin}\n1\n{}\n", STANDARD.encode(root_hash));
        let log_id = note_key_id(
            log_origin,
            NOTE_ED25519_SIGNATURE_TYPE,
            log_signing.verifying_key().as_bytes(),
        );
        let mut log_signature = log_id.to_vec();
        log_signature.extend_from_slice(&log_signing.sign(body.as_bytes()).to_bytes());
        let witnessed_at = now.timestamp();
        let witness_message = format!("cosignature/v1\ntime {witnessed_at}\n{body}");
        let witness_id = note_key_id(
            witness_name,
            NOTE_TIMESTAMPED_ED25519_COSIGNATURE_TYPE,
            witness_signing.verifying_key().as_bytes(),
        );
        let mut witness_signature = witness_id.to_vec();
        witness_signature.extend_from_slice(&u64::try_from(witnessed_at).unwrap().to_be_bytes());
        witness_signature
            .extend_from_slice(&witness_signing.sign(witness_message.as_bytes()).to_bytes());
        let checkpoint = format!(
            "{body}\n— {log_origin} {}\n— {witness_name} {}\n",
            STANDARD.encode(log_signature),
            STANDARD.encode(witness_signature)
        );
        let proof = ValidatedDriverReleaseTransparencyProof::new(DriverReleaseTransparencyProof {
            schema: Some("provider-driver-transparency-proof.schema.json".to_owned()),
            schema_version: 1,
            media_type: TRANSPARENCY_PROOF_MEDIA_TYPE.to_owned(),
            authorization_sha256: ToolDigest {
                algorithm: "sha256".to_owned(),
                value: authorization_sha256,
            },
            entries: vec![DriverReleaseTransparencyEntry {
                log_origin: log_origin.to_owned(),
                log_index: 0,
                inclusion_path: Vec::new(),
                checkpoint,
            }],
        })
        .unwrap();
        (root, authorization, proof)
    }

    #[test]
    fn verifies_rfc6962_inclusion_signed_checkpoint_and_timestamped_witness() {
        let now = Utc::now();
        let (root, authorization, proof) = fixture(now);
        let verified = proof.verify(&authorization, &root, now).unwrap();
        assert_eq!(verified.log_origins, vec!["log.example.test/deislet"]);
        assert_eq!(
            verified.witness_operators,
            vec!["witness-operator.example.test"]
        );
        assert_eq!(verified.earliest_witnessed_at.timestamp(), now.timestamp());
    }

    #[test]
    fn rejects_authorization_checkpoint_and_witness_tampering() {
        let now = Utc::now();
        let (root, current_authorization, proof) = fixture(now);

        let other = authorization(now + TimeDelta::seconds(1));
        assert!(proof.verify(&other, &root, now).is_err());

        let mut wrong_root = proof.get().clone();
        wrong_root.entries[0].checkpoint = wrong_root.entries[0].checkpoint.replacen(
            &STANDARD.encode(rfc6962_leaf_hash(
                &current_authorization.canonical_json().unwrap(),
            )),
            &STANDARD.encode([7_u8; 32]),
            1,
        );
        let wrong_root = ValidatedDriverReleaseTransparencyProof::new(wrong_root).unwrap();
        assert!(
            wrong_root
                .verify(&current_authorization, &root, now)
                .is_err()
        );

        let mut no_witness = proof.get().clone();
        no_witness.entries[0].checkpoint = no_witness.entries[0]
            .checkpoint
            .lines()
            .filter(|line| !line.starts_with("— witness."))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let no_witness = ValidatedDriverReleaseTransparencyProof::new(no_witness).unwrap();
        assert!(
            no_witness
                .verify(&current_authorization, &root, now)
                .is_err()
        );
    }

    #[test]
    fn trust_policy_separates_release_log_and_witness_authorities() {
        let now = Utc::now();
        let (root, _, _) = fixture(now);

        let mut release_controlled_log = root.get().clone();
        release_controlled_log
            .provider_driver_transparency
            .as_mut()
            .unwrap()
            .logs[0]
            .key_id = release_controlled_log.provider_driver_release.key_ids[0].clone();
        let error = ValidatedDriverReleaseTrustRoot::new(release_controlled_log).unwrap_err();
        assert!(error.to_string().contains("distinct from root and release"));

        let mut self_witnessing_log = root.get().clone();
        let log_operator = self_witnessing_log
            .provider_driver_transparency
            .as_ref()
            .unwrap()
            .logs[0]
            .operator
            .clone();
        self_witnessing_log
            .provider_driver_transparency
            .as_mut()
            .unwrap()
            .witnesses[0]
            .operator = log_operator;
        let error = ValidatedDriverReleaseTrustRoot::new(self_witnessing_log).unwrap_err();
        assert!(error.to_string().contains("independent operators"));
    }

    #[test]
    fn trust_policy_rejects_ambiguous_names_and_an_unreachable_log_threshold() {
        let now = Utc::now();
        let (_, first_log_key) = key(10, now);
        let (_, second_log_key) = key(11, now);
        let (_, witness_key) = key(12, now);
        let mut keys = vec![
            first_log_key.clone(),
            second_log_key.clone(),
            witness_key.clone(),
        ];
        keys.sort_by(|left, right| left.key_id.cmp(&right.key_id));
        let ambiguous = DriverReleaseTransparencyPolicy {
            log_operator_threshold: 1,
            witness_operator_threshold: 1,
            logs: vec![
                DriverReleaseTransparencyLog {
                    origin: "same-log.example.test/deislet".to_owned(),
                    operator: "a-log-operator.example.test".to_owned(),
                    key_id: first_log_key.key_id,
                },
                DriverReleaseTransparencyLog {
                    origin: "same-log.example.test/deislet".to_owned(),
                    operator: "b-log-operator.example.test".to_owned(),
                    key_id: second_log_key.key_id,
                },
            ],
            witnesses: vec![DriverReleaseTransparencyWitness {
                name: "witness.example.test/deislet".to_owned(),
                operator: "witness-operator.example.test".to_owned(),
                key_id: witness_key.key_id,
            }],
        };
        let error = validate_transparency_policy(&ambiguous, &keys).unwrap_err();
        assert!(error.to_string().contains("log origins must be unique"));

        let mut logs = Vec::new();
        let mut threshold_keys = Vec::new();
        for index in 0_u8..17 {
            let (_, trusted_key) = key(index + 20, now);
            logs.push(DriverReleaseTransparencyLog {
                origin: format!("log-{index:02}.example.test/deislet"),
                operator: format!("log-operator-{index:02}.example.test"),
                key_id: trusted_key.key_id.clone(),
            });
            threshold_keys.push(trusted_key);
        }
        let (_, threshold_witness_key) = key(50, now);
        threshold_keys.push(threshold_witness_key.clone());
        threshold_keys.sort_by(|left, right| left.key_id.cmp(&right.key_id));
        let unreachable = DriverReleaseTransparencyPolicy {
            log_operator_threshold: 17,
            witness_operator_threshold: 1,
            logs,
            witnesses: vec![DriverReleaseTransparencyWitness {
                name: "witness.example.test/deislet".to_owned(),
                operator: "witness-operator.example.test".to_owned(),
                key_id: threshold_witness_key.key_id,
            }],
        };
        let error = validate_transparency_policy(&unreachable, &threshold_keys).unwrap_err();
        assert!(error.to_string().contains("operator threshold is invalid"));
    }

    #[test]
    fn rfc6962_inclusion_handles_unbalanced_tree_paths() {
        let leaves = [b"a".as_slice(), b"b".as_slice(), b"c".as_slice()].map(rfc6962_leaf_hash);
        let left = rfc6962_node_hash(&leaves[0], &leaves[1]);
        let root = rfc6962_node_hash(&left, &leaves[2]);
        let path = vec![STANDARD.encode(leaves[0]), STANDARD.encode(leaves[2])];
        assert_eq!(inclusion_root(leaves[1], 1, 3, &path).unwrap(), root);
        let path = vec![STANDARD.encode(left)];
        assert_eq!(inclusion_root(leaves[2], 2, 3, &path).unwrap(), root);
    }

    #[test]
    fn rfc6962_inclusion_handles_balanced_and_larger_unbalanced_trees() {
        let leaves = [
            b"a".as_slice(),
            b"b".as_slice(),
            b"c".as_slice(),
            b"d".as_slice(),
            b"e".as_slice(),
        ]
        .map(rfc6962_leaf_hash);
        let left = rfc6962_node_hash(&leaves[0], &leaves[1]);
        let right = rfc6962_node_hash(&leaves[2], &leaves[3]);
        let root_four = rfc6962_node_hash(&left, &right);

        let paths_four = [
            vec![STANDARD.encode(leaves[1]), STANDARD.encode(right)],
            vec![STANDARD.encode(leaves[0]), STANDARD.encode(right)],
            vec![STANDARD.encode(leaves[3]), STANDARD.encode(left)],
            vec![STANDARD.encode(leaves[2]), STANDARD.encode(left)],
        ];
        for (index, path) in paths_four.iter().enumerate() {
            assert_eq!(
                inclusion_root(leaves[index], index as u64, 4, path).unwrap(),
                root_four
            );
        }

        let root_five = rfc6962_node_hash(&root_four, &leaves[4]);
        let first_path = vec![
            STANDARD.encode(leaves[1]),
            STANDARD.encode(right),
            STANDARD.encode(leaves[4]),
        ];
        assert_eq!(
            inclusion_root(leaves[0], 0, 5, &first_path).unwrap(),
            root_five
        );
        let last_path = vec![STANDARD.encode(root_four)];
        assert_eq!(
            inclusion_root(leaves[4], 4, 5, &last_path).unwrap(),
            root_five
        );

        assert!(inclusion_root(leaves[0], 0, 5, &first_path[..2]).is_err());
        let mut too_long = last_path;
        too_long.push(STANDARD.encode(leaves[0]));
        assert!(inclusion_root(leaves[4], 4, 5, &too_long).is_err());
    }

    #[test]
    fn rfc6962_inclusion_matches_reference_trees_through_64_leaves() {
        let all_leaves = (0_u8..64)
            .map(|value| rfc6962_leaf_hash(&[value]))
            .collect::<Vec<_>>();
        for tree_size in 1..=all_leaves.len() {
            let leaves = &all_leaves[..tree_size];
            let expected = reference_merkle_root(leaves);
            for (index, leaf) in leaves.iter().copied().enumerate() {
                let mut path = Vec::new();
                reference_inclusion_path(leaves, index, &mut path);
                assert_eq!(
                    inclusion_root(leaf, index as u64, tree_size as u64, &path).unwrap(),
                    expected,
                    "tree size {tree_size}, leaf {index}"
                );
            }
        }
    }
}
