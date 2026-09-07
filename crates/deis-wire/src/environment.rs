//! Shared Edge Canon environment declaration and deployment-snapshot model.
//!
//! The same structs are used by the compiler, native bundle, control plane and
//! runtime. That is intentional: a declaration accepted while building must
//! not acquire a second interpretation when a deployment resolves secrets or
//! when a node materializes `context.env`.

use std::collections::{BTreeMap, BTreeSet};

use crate::conventions;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const ENVIRONMENT_DECLARATIONS_FORMAT: &str = "edge-canon.environment-secrets/v1";
pub const ENVIRONMENT_BINDING_SNAPSHOT_FORMAT: &str = "edge-canon.environment-binding-snapshot/v1";
pub const ENVIRONMENT_DECLARATIONS_SCHEMA: &str =
    "https://github.com/deislet/edge-canon/schemas/environment-secrets.schema.json";
pub const ENVIRONMENT_BINDING_SNAPSHOT_SCHEMA: &str =
    "https://github.com/deislet/edge-canon/schemas/environment-binding-snapshot.schema.json";
pub const ENVIRONMENT_BINDING_COUNT_LIMIT: usize = 64;
pub const ENVIRONMENT_VALUE_BYTES_LIMIT: usize = 5_120;
/// The exact Edge Canon revision this product build implements. Documents
/// naming another syntactically valid commit are future input, not a request
/// to guess compatible behavior.
pub const CURRENT_EDGE_CANON_STANDARD_VERSION: &str =
    "edge-canon.next@a41a4caadeaf8e5751ebe57ef5be673d18384654";
/// SHA-256 of the exact standard contract at [`CURRENT_EDGE_CANON_STANDARD_VERSION`].
/// Artifact records carry this independently from the source revision so an
/// operator can prove which bytes were used even when a repository checkout
/// is unavailable.
pub const CURRENT_EDGE_CANON_CONTRACT_SHA256: &str =
    "926fc7a3fa67426b1cb3c3447949492f5172c18f25d7895f3a67f8b3437acbb8";
/// SHA-256 of the requirements registry used by this product build.
pub const CURRENT_EDGE_CANON_REQUIREMENTS_SHA256: &str =
    "399407bd897f76d9c7ac6e36f4b4ad33934cbec57121d04537413a685dd809cc";
/// SHA-256 of the canonical Build Output JSON Schema used by every builder,
/// control-plane ingestion path and provider packager in this build.
pub const CURRENT_EDGE_CANON_ARTIFACT_SCHEMA_SHA256: &str =
    "2711f0fd63103f9e40b5a2909279e16e9025db6b8616ab5dd4f1265d3ccb5191";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentError {
    code: &'static str,
    detail: &'static str,
}

impl EnvironmentError {
    fn new(code: &'static str, detail: &'static str) -> Self {
        Self { code, detail }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl std::fmt::Display for EnvironmentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for EnvironmentError {}

pub type EnvironmentResult<T> = Result<T, EnvironmentError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentDeclarations {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub schema_version: u32,
    pub format: String,
    pub standard_version: String,
    pub access: EnvironmentAccess,
    pub limits: EnvironmentLimits,
    pub declarations: Vec<EnvironmentDeclaration>,
}

impl EnvironmentDeclarations {
    pub fn new(
        standard_version: impl Into<String>,
        mut declarations: Vec<EnvironmentDeclaration>,
    ) -> Self {
        declarations.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
        Self {
            schema: Some(ENVIRONMENT_DECLARATIONS_SCHEMA.to_string()),
            schema_version: 1,
            format: ENVIRONMENT_DECLARATIONS_FORMAT.to_string(),
            standard_version: standard_version.into(),
            access: EnvironmentAccess::default(),
            limits: EnvironmentLimits::default(),
            declarations,
        }
    }

    pub fn validate(
        &self,
        expected_standard_version: &str,
        resource_names: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> EnvironmentResult<()> {
        if self.schema_version != 1 || self.format != ENVIRONMENT_DECLARATIONS_FORMAT {
            return Err(EnvironmentError::new(
                "EC_ENV_VERSION_UNSUPPORTED",
                "environment declaration document version is unsupported",
            ));
        }
        validate_standard_version(&self.standard_version, expected_standard_version)?;
        if self.access != EnvironmentAccess::default()
            || self.limits != EnvironmentLimits::default()
        {
            return Err(EnvironmentError::new(
                "EC_ENV_DOCUMENT_INVALID",
                "environment access or limits differ from the v1 contract",
            ));
        }
        if self.declarations.len() > ENVIRONMENT_BINDING_COUNT_LIMIT {
            return Err(EnvironmentError::new(
                "EC_ENV_BINDING_LIMIT_EXCEEDED",
                "environment declaration count exceeds the v1 limit",
            ));
        }

        let resources = resource_names
            .into_iter()
            .map(|name| name.as_ref().to_string())
            .collect::<BTreeSet<_>>();
        let mut names = BTreeSet::new();
        for declaration in &self.declarations {
            declaration.validate()?;
            if !names.insert(declaration.name.as_str()) {
                return Err(EnvironmentError::new(
                    "EC_ENV_DUPLICATE_NAME",
                    "environment declarations contain a duplicate name",
                ));
            }
            if resources.contains(&declaration.name) {
                return Err(EnvironmentError::new(
                    "EC_ENV_NAMESPACE_COLLISION",
                    "environment and logical-resource declarations share a name",
                ));
            }
        }
        Ok(())
    }

    /// SHA-256 of RFC 8785 canonical UTF-8 bytes, without a presentation LF.
    pub fn identity(&self) -> EnvironmentResult<String> {
        let bytes = self.canonical_json()?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    pub fn canonical_json(&self) -> EnvironmentResult<Vec<u8>> {
        canonical_bytes(self)
    }

    pub fn by_name(&self) -> BTreeMap<&str, &EnvironmentDeclaration> {
        self.declarations
            .iter()
            .map(|declaration| (declaration.name.as_str(), declaration))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentAccess {
    pub surface: String,
    pub extra_provider_bindings: String,
}

impl Default for EnvironmentAccess {
    fn default() -> Self {
        Self {
            surface: "context.env".to_string(),
            extra_provider_bindings: "excluded".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentLimits {
    pub binding_count: usize,
    pub value_bytes: usize,
    pub measurement: String,
}

impl Default for EnvironmentLimits {
    fn default() -> Self {
        Self {
            binding_count: ENVIRONMENT_BINDING_COUNT_LIMIT,
            value_bytes: ENVIRONMENT_VALUE_BYTES_LIMIT,
            measurement: "utf-8".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentDeclaration {
    pub name: String,
    pub kind: EnvironmentKind,
    pub value_type: EnvironmentValueType,
    pub required: bool,
}

impl EnvironmentDeclaration {
    pub fn validate(&self) -> EnvironmentResult<()> {
        if !valid_name(&self.name) {
            return Err(EnvironmentError::new(
                "EC_ENV_NAME_INVALID",
                "environment binding name is outside the v1 portable grammar",
            ));
        }
        // A declaration is a name on `context.env`, and some of those names
        // are the platform's. It is refused rather than shadowed, because
        // shadowing has no honest outcome: the platform installs its own
        // property after the declared values are copied, so the author's value
        // is never readable, while the declaration goes on saying a value is
        // required and the deployment goes on demanding one for it. The same
        // sentence is already refused on the other side of the environment
        // table - `deis_bundle::Bindings::validate` refuses a logical resource
        // called ASSETS - and this is the half that covers a variable or a
        // secret.
        //
        // Here, on the declaration itself, so that every reader of the
        // document reaches the same verdict: the compiler when it writes one,
        // the control plane when a bundle is uploaded, and a node when a
        // deployment is loaded - including a bundle that reached that node
        // without passing through any control plane of ours.
        if self
            .name
            .eq_ignore_ascii_case(conventions::RESERVED_PLATFORM_BINDING_NAME)
        {
            return Err(EnvironmentError::new(
                "EC_ENV_NAMESPACE_COLLISION",
                "an environment declaration is named ASSETS, which is the platform's own binding \
                 for this version's static files; the platform's property wins, so the declared \
                 value could never be read and env.ASSETS.fetch() would still be the platform's",
            ));
        }
        if self
            .name
            .to_ascii_uppercase()
            .starts_with(conventions::RESERVED_PLATFORM_PREFIX)
        {
            return Err(EnvironmentError::new(
                "EC_ENV_NAMESPACE_COLLISION",
                "an environment declaration begins with DEIS_, the prefix the platform carries a \
                 deployment's own wiring under; such a name is lifted back out of the map before \
                 it becomes context.env, so the declared value could never be read",
            ));
        }
        if self.kind == EnvironmentKind::Secret && self.value_type != EnvironmentValueType::String {
            return Err(EnvironmentError::new(
                "EC_ENV_DECLARATION_INVALID",
                "v1 secrets must have string values",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentKind {
    Config,
    Secret,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentValueType {
    String,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentBindingSnapshot {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub schema_version: u32,
    pub format: String,
    pub standard_version: String,
    pub deployment_version_id: String,
    pub environment_id: String,
    pub declarations_sha256: String,
    pub activation: EnvironmentActivation,
    pub bindings: Vec<EnvironmentBinding>,
}

impl EnvironmentBindingSnapshot {
    pub fn new(
        standard_version: impl Into<String>,
        deployment_version_id: impl Into<String>,
        environment_id: impl Into<String>,
        declarations_sha256: impl Into<String>,
        mut bindings: Vec<EnvironmentBinding>,
    ) -> Self {
        bindings.sort_by(|left, right| left.name().as_bytes().cmp(right.name().as_bytes()));
        Self {
            schema: Some(ENVIRONMENT_BINDING_SNAPSHOT_SCHEMA.to_string()),
            schema_version: 1,
            format: ENVIRONMENT_BINDING_SNAPSHOT_FORMAT.to_string(),
            standard_version: standard_version.into(),
            deployment_version_id: deployment_version_id.into(),
            environment_id: environment_id.into(),
            declarations_sha256: declarations_sha256.into(),
            activation: EnvironmentActivation::default(),
            bindings,
        }
    }

    pub fn validate(
        &self,
        declarations: &EnvironmentDeclarations,
        expected_standard_version: &str,
        secret_revision_available: impl Fn(&str) -> bool,
    ) -> EnvironmentResult<()> {
        declarations.validate(expected_standard_version, std::iter::empty::<&str>())?;
        if self.schema_version != 1 || self.format != ENVIRONMENT_BINDING_SNAPSHOT_FORMAT {
            return Err(EnvironmentError::new(
                "EC_ENV_VERSION_UNSUPPORTED",
                "environment binding snapshot version is unsupported",
            ));
        }
        validate_standard_version(&self.standard_version, expected_standard_version)?;
        if !valid_identity(&self.deployment_version_id)
            || !valid_identity(&self.environment_id)
            || self.activation != EnvironmentActivation::default()
        {
            return Err(EnvironmentError::new(
                "EC_ENV_SNAPSHOT_INVALID",
                "environment binding snapshot identity or activation policy is invalid",
            ));
        }
        if self.declarations_sha256 != declarations.identity()? {
            return Err(EnvironmentError::new(
                "EC_ENV_DECLARATION_IDENTITY_MISMATCH",
                "snapshot does not bind the supplied declaration document",
            ));
        }
        if self.bindings.len() > ENVIRONMENT_BINDING_COUNT_LIMIT {
            return Err(EnvironmentError::new(
                "EC_ENV_BINDING_LIMIT_EXCEEDED",
                "environment binding count exceeds the v1 limit",
            ));
        }

        let declared = declarations.by_name();
        let mut names = BTreeSet::new();
        for binding in &self.bindings {
            if !valid_name(binding.name()) {
                return Err(EnvironmentError::new(
                    "EC_ENV_BINDING_INVALID",
                    "snapshot contains an invalid binding name",
                ));
            }
            if !names.insert(binding.name()) {
                return Err(EnvironmentError::new(
                    "EC_ENV_DUPLICATE_NAME",
                    "snapshot contains a duplicate binding name",
                ));
            }
            let declaration = declared.get(binding.name()).ok_or_else(|| {
                EnvironmentError::new(
                    "EC_ENV_BINDING_UNDECLARED",
                    "snapshot contains a binding absent from the declaration",
                )
            })?;
            if binding.kind() != declaration.kind || binding.value_type() != declaration.value_type
            {
                return Err(EnvironmentError::new(
                    "EC_ENV_BINDING_TYPE_MISMATCH",
                    "snapshot binding kind or value type differs from its declaration",
                ));
            }
            match binding {
                EnvironmentBinding::Config(binding) => binding.validate()?,
                EnvironmentBinding::Secret(binding) => {
                    if !valid_identity(&binding.revision) {
                        return Err(EnvironmentError::new(
                            "EC_ENV_BINDING_INVALID",
                            "secret binding revision identity is invalid",
                        ));
                    }
                    if !secret_revision_available(&binding.revision) {
                        return Err(EnvironmentError::new(
                            "EC_ENV_SECRET_REVISION_UNAVAILABLE",
                            "secret binding revision is unavailable",
                        ));
                    }
                }
            }
        }
        if declarations
            .declarations
            .iter()
            .any(|declaration| declaration.required && !names.contains(declaration.name.as_str()))
        {
            return Err(EnvironmentError::new(
                "EC_ENV_REQUIRED_MISSING",
                "snapshot omits a required environment binding",
            ));
        }
        Ok(())
    }

    pub fn identity(&self) -> EnvironmentResult<String> {
        let bytes = self.canonical_json()?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    pub fn canonical_json(&self) -> EnvironmentResult<Vec<u8>> {
        canonical_bytes(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentActivation {
    pub mode: String,
    pub missing_required: String,
    pub unavailable_secret_revision: String,
}

impl Default for EnvironmentActivation {
    fn default() -> Self {
        Self {
            mode: "version-bound-atomic".to_string(),
            missing_required: "reject".to_string(),
            unavailable_secret_revision: "reject".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum EnvironmentBinding {
    Config(EnvironmentConfigBinding),
    Secret(EnvironmentSecretBinding),
}

impl EnvironmentBinding {
    pub fn name(&self) -> &str {
        match self {
            Self::Config(binding) => &binding.name,
            Self::Secret(binding) => &binding.name,
        }
    }

    pub fn kind(&self) -> EnvironmentKind {
        match self {
            Self::Config(binding) => binding.kind,
            Self::Secret(binding) => binding.kind,
        }
    }

    pub fn value_type(&self) -> EnvironmentValueType {
        match self {
            Self::Config(binding) => binding.value_type,
            Self::Secret(binding) => binding.value_type,
        }
    }

    pub fn revision(&self) -> &str {
        match self {
            Self::Config(binding) => &binding.revision,
            Self::Secret(binding) => &binding.revision,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentConfigBinding {
    pub name: String,
    pub kind: EnvironmentKind,
    pub value_type: EnvironmentValueType,
    pub revision: String,
    pub value: Value,
}

impl EnvironmentConfigBinding {
    pub fn new(
        name: impl Into<String>,
        value_type: EnvironmentValueType,
        value: Value,
    ) -> EnvironmentResult<Self> {
        let revision = config_revision(&value, value_type)?;
        Ok(Self {
            name: name.into(),
            kind: EnvironmentKind::Config,
            value_type,
            revision,
            value,
        })
    }

    fn validate(&self) -> EnvironmentResult<()> {
        if self.kind != EnvironmentKind::Config || !valid_identity(&self.revision) {
            return Err(EnvironmentError::new(
                "EC_ENV_BINDING_INVALID",
                "config binding kind or revision identity is invalid",
            ));
        }
        let expected = config_revision(&self.value, self.value_type)?;
        if self.revision != expected {
            return Err(EnvironmentError::new(
                "EC_ENV_CONFIG_REVISION_MISMATCH",
                "config binding revision does not match its value",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentSecretBinding {
    pub name: String,
    pub kind: EnvironmentKind,
    pub value_type: EnvironmentValueType,
    pub revision: String,
}

impl EnvironmentSecretBinding {
    pub fn new(name: impl Into<String>, revision: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: EnvironmentKind::Secret,
            value_type: EnvironmentValueType::String,
            revision: revision.into(),
        }
    }
}

pub fn config_revision(
    value: &Value,
    value_type: EnvironmentValueType,
) -> EnvironmentResult<String> {
    let bytes = match value_type {
        EnvironmentValueType::String => value
            .as_str()
            .map(str::as_bytes)
            .ok_or_else(|| {
                EnvironmentError::new(
                    "EC_ENV_VALUE_INVALID",
                    "config/string binding value is not a string",
                )
            })?
            .to_vec(),
        EnvironmentValueType::Json => {
            if value.is_string() {
                return Err(EnvironmentError::new(
                    "EC_ENV_VALUE_INVALID",
                    "config/json binding value is a string",
                ));
            }
            canonical_bytes(value)?
        }
    };
    if bytes.len() > ENVIRONMENT_VALUE_BYTES_LIMIT {
        return Err(EnvironmentError::new(
            "EC_ENV_VALUE_LIMIT_EXCEEDED",
            "environment value exceeds the v1 UTF-8 byte limit",
        ));
    }
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub fn validate_secret_value(value: &str) -> EnvironmentResult<()> {
    if value.len() > ENVIRONMENT_VALUE_BYTES_LIMIT {
        return Err(EnvironmentError::new(
            "EC_ENV_VALUE_LIMIT_EXCEEDED",
            "secret value exceeds the v1 UTF-8 byte limit",
        ));
    }
    Ok(())
}

fn canonical_bytes(value: &impl Serialize) -> EnvironmentResult<Vec<u8>> {
    serde_jcs::to_vec(value).map_err(|_| {
        EnvironmentError::new(
            "EC_ENV_VALUE_INVALID",
            "value cannot be represented as RFC 8785 canonical JSON",
        )
    })
}

fn validate_standard_version(actual: &str, expected: &str) -> EnvironmentResult<()> {
    if !valid_standard_version(actual) || actual != expected {
        return Err(EnvironmentError::new(
            "EC_ENV_STANDARD_PIN_INVALID",
            "environment document does not use the expected exact standard version",
        ));
    }
    Ok(())
}

fn valid_standard_version(value: &str) -> bool {
    let Some(commit) = value.strip_prefix("edge-canon.next@") else {
        return false;
    };
    commit.len() == 40
        && commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_uppercase()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
}

fn valid_identity(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes.iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b':' | b'@' | b'/' | b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STANDARD: &str = "edge-canon.next@a41a4caadeaf8e5751ebe57ef5be673d18384654";

    fn declarations() -> EnvironmentDeclarations {
        EnvironmentDeclarations::new(
            STANDARD,
            vec![
                EnvironmentDeclaration {
                    name: "OPTIONS".into(),
                    kind: EnvironmentKind::Config,
                    value_type: EnvironmentValueType::Json,
                    required: false,
                },
                EnvironmentDeclaration {
                    name: "PUBLIC_NAME".into(),
                    kind: EnvironmentKind::Config,
                    value_type: EnvironmentValueType::String,
                    required: true,
                },
                EnvironmentDeclaration {
                    name: "TOKEN".into(),
                    kind: EnvironmentKind::Secret,
                    value_type: EnvironmentValueType::String,
                    required: true,
                },
            ],
        )
    }

    #[test]
    fn document_is_sorted_valid_and_has_a_stable_identity() {
        let document = declarations();
        document.validate(STANDARD, ["CACHE"]).unwrap();
        assert_eq!(document.declarations[0].name, "OPTIONS");
        assert_eq!(document.identity().unwrap(), document.identity().unwrap());
    }

    #[test]
    fn declarations_reject_duplicates_namespace_collisions_and_secret_json() {
        let mut duplicate = declarations();
        duplicate
            .declarations
            .push(duplicate.declarations[0].clone());
        assert_eq!(
            duplicate.validate(STANDARD, ["CACHE"]).unwrap_err().code(),
            "EC_ENV_DUPLICATE_NAME"
        );

        assert_eq!(
            declarations()
                .validate(STANDARD, ["TOKEN"])
                .unwrap_err()
                .code(),
            "EC_ENV_NAMESPACE_COLLISION"
        );

        let mut invalid = declarations();
        invalid.declarations[2].value_type = EnvironmentValueType::Json;
        assert_eq!(
            invalid.validate(STANDARD, ["CACHE"]).unwrap_err().code(),
            "EC_ENV_DECLARATION_INVALID"
        );
    }

    /// A declaration may not take a name the platform publishes on
    /// `context.env` itself. Both halves are checked at the document level,
    /// because that is the call every reader makes: the compiler when it
    /// writes the document, deis-bundle when a bundle is uploaded or loaded,
    /// and a node when it materializes the environment. Without this, the
    /// name would be minted as a required declaration, every layer would
    /// demand a value for it, and the value would then be unreadable - the
    /// platform's own property is installed last and wins.
    #[test]
    fn a_declaration_may_not_take_a_platform_name() {
        for name in [
            conventions::RESERVED_PLATFORM_BINDING_NAME,
            "DEIS_STORE_ENDPOINT",
        ] {
            let mut taken = declarations();
            taken.declarations.push(EnvironmentDeclaration {
                name: name.to_string(),
                kind: EnvironmentKind::Config,
                value_type: EnvironmentValueType::String,
                required: true,
            });
            let error = taken.validate(STANDARD, ["CACHE"]).unwrap_err();
            assert_eq!(error.code(), "EC_ENV_NAMESPACE_COLLISION", "{name}");
            // The name is in the sentence, not only in the code: what an
            // author sees is a compiler's stderr line, and a refusal that did
            // not say which name it meant would send them looking through
            // every declaration they wrote.
            assert!(
                error.to_string().contains(name)
                    || error
                        .to_string()
                        .contains(conventions::RESERVED_PLATFORM_PREFIX),
                "the refusal for {name:?} does not name it: {error}"
            );
        }
    }

    #[test]
    fn snapshot_binds_exact_config_values_and_secret_revisions() {
        let declarations = declarations();
        let config = EnvironmentConfigBinding::new(
            "PUBLIC_NAME",
            EnvironmentValueType::String,
            Value::String("edge".into()),
        )
        .unwrap();
        let snapshot = EnvironmentBindingSnapshot::new(
            STANDARD,
            "version-1",
            "environment-1",
            declarations.identity().unwrap(),
            vec![
                EnvironmentBinding::Secret(EnvironmentSecretBinding::new("TOKEN", "secret-7")),
                EnvironmentBinding::Config(config),
            ],
        );
        snapshot
            .validate(&declarations, STANDARD, |revision| revision == "secret-7")
            .unwrap();
        assert_eq!(snapshot.bindings[0].name(), "PUBLIC_NAME");

        let mut changed = snapshot.clone();
        let EnvironmentBinding::Config(binding) = &mut changed.bindings[0] else {
            panic!("first binding is config")
        };
        binding.value = Value::String("changed".into());
        assert_eq!(
            changed
                .validate(&declarations, STANDARD, |_| true)
                .unwrap_err()
                .code(),
            "EC_ENV_CONFIG_REVISION_MISMATCH"
        );
    }

    #[test]
    fn snapshot_rejects_missing_required_and_unavailable_secret() {
        let declarations = declarations();
        let config = EnvironmentConfigBinding::new(
            "PUBLIC_NAME",
            EnvironmentValueType::String,
            Value::String("edge".into()),
        )
        .unwrap();
        let missing = EnvironmentBindingSnapshot::new(
            STANDARD,
            "version-1",
            "environment-1",
            declarations.identity().unwrap(),
            vec![EnvironmentBinding::Config(config)],
        );
        assert_eq!(
            missing
                .validate(&declarations, STANDARD, |_| true)
                .unwrap_err()
                .code(),
            "EC_ENV_REQUIRED_MISSING"
        );

        let mut unavailable = missing;
        unavailable
            .bindings
            .push(EnvironmentBinding::Secret(EnvironmentSecretBinding::new(
                "TOKEN", "gone",
            )));
        assert_eq!(
            unavailable
                .validate(&declarations, STANDARD, |_| false)
                .unwrap_err()
                .code(),
            "EC_ENV_SECRET_REVISION_UNAVAILABLE"
        );
    }

    #[test]
    fn exact_utf8_limits_are_enforced() {
        let exact = "x".repeat(ENVIRONMENT_VALUE_BYTES_LIMIT);
        validate_secret_value(&exact).unwrap();
        assert_eq!(
            validate_secret_value(&(exact + "x")).unwrap_err().code(),
            "EC_ENV_VALUE_LIMIT_EXCEEDED"
        );
    }

    #[test]
    fn compiled_standard_pin_matches_the_repository_lock() {
        let lock: serde_json::Value =
            serde_json::from_str(include_str!("../../../standards/edge-canon.lock.json")).unwrap();
        let contract = lock["standard"]["contractId"].as_str().unwrap();
        let commit = lock["source"]["sourceCommit"].as_str().unwrap();
        assert_eq!(
            CURRENT_EDGE_CANON_STANDARD_VERSION,
            format!("{contract}@{commit}"),
            "the compiled protocol pin drifted from standards/edge-canon.lock.json"
        );
        assert_eq!(
            CURRENT_EDGE_CANON_CONTRACT_SHA256,
            lock["source"]["contractSha256"].as_str().unwrap(),
            "the compiled contract digest drifted from the standard lock"
        );
        assert_eq!(
            CURRENT_EDGE_CANON_REQUIREMENTS_SHA256,
            lock["source"]["requirementsSha256"].as_str().unwrap(),
            "the compiled requirements digest drifted from the standard lock"
        );
        assert_eq!(
            CURRENT_EDGE_CANON_ARTIFACT_SCHEMA_SHA256,
            lock["source"]["canonicalArtifactSchemaSha256"]
                .as_str()
                .unwrap(),
            "the compiled canonical artifact schema digest drifted from the standard lock"
        );
    }
}
