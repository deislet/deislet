use std::{collections::BTreeSet, fmt};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::{ALL_DRIVER_OPERATIONS, DriverOperation, Sha256Digest};

pub const DRIVER_CONTRACT_VERSION: &str = "1.0.0-draft.1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ToolDigest {
    pub algorithm: String,
    pub value: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverIdentity {
    pub id: String,
    pub version: String,
    pub distribution_digest: ToolDigest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct BackendManifest {
    pub id: String,
    pub product: String,
    pub api_version: String,
    pub compatibility_date: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct SupportedStandard {
    pub contract_id: String,
    pub version: String,
    pub contract_digest: ToolDigest,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OfficialToolKind {
    Cli,
    Api,
    Sdk,
    Native,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct OfficialTool {
    pub kind: OfficialToolKind,
    pub name: String,
    pub version: String,
    pub distribution_digest: ToolDigest,
    pub non_interactive: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct CredentialPolicy {
    pub allowed: Vec<String>,
    pub argv_allowed: bool,
    pub persist_plaintext_allowed: bool,
    pub runtime_receives_management_credential: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicy {
    pub non_interactive: bool,
    pub idempotent: bool,
    pub crash_recoverable: bool,
    pub structured_output: bool,
    pub query_before_retry: bool,
    pub redacts_diagnostics: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImplementationKind {
    Native,
    Composed,
    Companion,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ImplementationMapping {
    pub capability_family: String,
    pub implementation: ImplementationKind,
    pub components: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ClaimPolicy {
    pub certification_source: String,
    pub artifact_generation_is_compliance: bool,
    pub syntax_verification_is_compliance: bool,
    pub deployment_is_compliance: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ProviderDriverManifest {
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    pub schema_version: u32,
    pub driver_contract_version: String,
    pub driver: DriverIdentity,
    pub backend: BackendManifest,
    pub supported_standard_versions: Vec<SupportedStandard>,
    pub operations: Vec<DriverOperation>,
    pub official_tools: Vec<OfficialTool>,
    pub credential_transport: CredentialPolicy,
    pub execution: ExecutionPolicy,
    pub implementation_mappings: Vec<ImplementationMapping>,
    pub claim_policy: ClaimPolicy,
    pub supported_host_operating_systems: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ValidatedProviderDriverManifest(ProviderDriverManifest);

impl ValidatedProviderDriverManifest {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, DriverManifestError> {
        let manifest: ProviderDriverManifest =
            serde_json::from_slice(bytes).map_err(DriverManifestError::Json)?;
        Self::new(manifest)
    }

    pub fn new(manifest: ProviderDriverManifest) -> Result<Self, DriverManifestError> {
        validate(&manifest)?;
        Ok(Self(manifest))
    }

    #[must_use]
    pub fn get(&self) -> &ProviderDriverManifest {
        &self.0
    }
}

#[derive(Debug)]
pub enum DriverManifestError {
    Json(serde_json::Error),
    Invalid(&'static str),
}

impl fmt::Display for DriverManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => {
                write!(formatter, "invalid provider driver manifest JSON: {error}")
            }
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DriverManifestError {}

fn digest_is_sha256(digest: &ToolDigest) -> bool {
    digest.algorithm == "sha256"
}

fn unique_nonempty(values: &[String]) -> bool {
    !values.is_empty()
        && values.iter().all(|value| !value.is_empty())
        && values.iter().collect::<BTreeSet<_>>().len() == values.len()
}

fn is_contract_identifier(value: &str) -> bool {
    let mut saw_separator = false;
    let mut previous_separator = true;
    for byte in value.bytes() {
        let separator = matches!(byte, b'.' | b'-');
        if !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || separator)
            || (separator && previous_separator)
        {
            return false;
        }
        saw_separator |= separator;
        previous_separator = separator;
    }
    saw_separator && !previous_separator
}

fn is_kebab_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn is_schema_semver(value: &str) -> bool {
    let (core, suffix) = value
        .split_once('-')
        .map_or((value, None), |(core, suffix)| (core, Some(suffix)));
    core.split('.').count() == 3
        && core
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && suffix.is_none_or(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        })
}

fn validate(manifest: &ProviderDriverManifest) -> Result<(), DriverManifestError> {
    macro_rules! require {
        ($condition:expr, $message:literal) => {
            if !$condition {
                return Err(DriverManifestError::Invalid($message));
            }
        };
    }

    require!(
        manifest.schema_version == 1,
        "provider driver schemaVersion must be 1"
    );
    require!(
        manifest
            .schema
            .as_deref()
            .is_some_and(|schema| !schema.is_empty()),
        "provider driver manifest must identify its JSON schema"
    );
    require!(
        manifest.driver_contract_version == DRIVER_CONTRACT_VERSION,
        "provider driver contract version is not the executable v1 draft"
    );
    require!(
        is_contract_identifier(&manifest.driver.id),
        "provider driver ID must be a lowercase dotted or dashed identifier"
    );
    require!(
        is_schema_semver(&manifest.driver.version),
        "provider driver version must use the schema semver shape"
    );
    require!(
        digest_is_sha256(&manifest.driver.distribution_digest),
        "provider driver distribution digest must use sha256"
    );
    require!(
        matches!(
            manifest.backend.id.as_str(),
            "deislet" | "cloudflare-workers-pages" | "tencent-edgeone-makers" | "deno"
        ) && !manifest.backend.product.is_empty()
            && !manifest.backend.api_version.is_empty(),
        "provider backend ID or metadata is outside the v1 contract"
    );
    require!(
        manifest
            .backend
            .compatibility_date
            .as_deref()
            .is_none_or(|date| { NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok() }),
        "provider compatibility date must be a real calendar date"
    );
    require!(
        manifest.operations == ALL_DRIVER_OPERATIONS,
        "provider driver must implement all 12 lifecycle operations in canonical order"
    );
    require!(
        !manifest.official_tools.is_empty(),
        "provider driver must pin an official tool"
    );
    require!(
        manifest.official_tools.iter().all(|tool| {
            tool.non_interactive
                && !tool.name.is_empty()
                && !tool.version.is_empty()
                && digest_is_sha256(&tool.distribution_digest)
        }),
        "official tools must be named, pinned to a version and sha256, and non-interactive"
    );
    require!(
        unique_nonempty(&manifest.credential_transport.allowed)
            && manifest
                .credential_transport
                .allowed
                .iter()
                .all(|transport| {
                    matches!(
                        transport.as_str(),
                        "stdin"
                            | "restricted-temporary-file"
                            | "system-keyring"
                            | "short-lived-environment"
                    )
                }),
        "provider credential transports must be non-empty and unique"
    );
    require!(
        !manifest.credential_transport.argv_allowed
            && !manifest.credential_transport.persist_plaintext_allowed
            && !manifest
                .credential_transport
                .runtime_receives_management_credential,
        "provider credentials may not use argv, plaintext persistence, or application runtime"
    );
    let execution = &manifest.execution;
    require!(
        execution.non_interactive
            && execution.idempotent
            && execution.crash_recoverable
            && execution.structured_output
            && execution.query_before_retry
            && execution.redacts_diagnostics,
        "provider execution policy must retain every v1 safety guarantee"
    );
    require!(
        !manifest.implementation_mappings.is_empty()
            && manifest.implementation_mappings.iter().all(|mapping| {
                is_kebab_identifier(&mapping.capability_family)
                    && unique_nonempty(&mapping.components)
            }),
        "provider driver needs at least one implementation mapping"
    );
    let claims = &manifest.claim_policy;
    require!(
        claims.certification_source == "edge-canon-conformance-registry"
            && !claims.artifact_generation_is_compliance
            && !claims.syntax_verification_is_compliance
            && !claims.deployment_is_compliance,
        "provider driver may not self-assert Edge Canon compliance"
    );
    require!(
        unique_nonempty(&manifest.supported_host_operating_systems)
            && manifest
                .supported_host_operating_systems
                .iter()
                .all(|host| {
                    matches!(
                        host.as_str(),
                        "linux-amd64" | "linux-arm64" | "macos-arm64" | "windows-amd64"
                    )
                }),
        "provider driver host operating systems must be non-empty and unique"
    );
    require!(
        manifest.supported_standard_versions.iter().all(|standard| {
            standard.contract_id.starts_with("edge-canon.")
                && is_contract_identifier(&standard.contract_id)
                && !standard.version.is_empty()
                && digest_is_sha256(&standard.contract_digest)
        }),
        "supported standards must carry an ID, version and sha256"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] =
        include_bytes!("../../../contracts/v1/fixtures/valid/provider-driver.json");

    #[test]
    fn shared_contract_fixture_is_the_executable_manifest_shape() {
        let manifest = ValidatedProviderDriverManifest::parse_json(FIXTURE).unwrap();
        assert_eq!(manifest.get().operations, ALL_DRIVER_OPERATIONS);
        assert!(manifest.get().supported_standard_versions.is_empty());
    }

    #[test]
    fn refuses_a_partial_driver_even_when_json_schema_would_reject_it_too() {
        let mut manifest: ProviderDriverManifest = serde_json::from_slice(FIXTURE).unwrap();
        manifest.operations.pop();
        assert!(matches!(
            ValidatedProviderDriverManifest::new(manifest),
            Err(DriverManifestError::Invalid(message))
                if message.contains("all 12 lifecycle operations")
        ));
    }

    #[test]
    fn refuses_weakened_credential_or_reconciliation_guarantees() {
        let mut manifest: ProviderDriverManifest = serde_json::from_slice(FIXTURE).unwrap();
        manifest.credential_transport.argv_allowed = true;
        assert!(ValidatedProviderDriverManifest::new(manifest).is_err());

        let mut manifest: ProviderDriverManifest = serde_json::from_slice(FIXTURE).unwrap();
        manifest.execution.query_before_retry = false;
        assert!(ValidatedProviderDriverManifest::new(manifest).is_err());
    }

    #[test]
    fn refuses_unknown_manifest_fields_instead_of_ignoring_them() {
        let mut document: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
        document["credentialTransport"]["argvAlias"] = serde_json::json!(true);
        let bytes = serde_json::to_vec(&document).unwrap();
        assert!(matches!(
            ValidatedProviderDriverManifest::parse_json(&bytes),
            Err(DriverManifestError::Json(_))
        ));
    }
}
