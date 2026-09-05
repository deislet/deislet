use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    OfficialToolKind, ProviderDriverManifest, Sha256Digest, ToolDigest,
    ValidatedProviderDriverManifest, manifest::DRIVER_CONTRACT_VERSION,
};

const MAX_DISTRIBUTIONS: usize = 4;
const MAX_COMPONENTS: usize = 32;
const MAX_FILES: usize = 100_000;
const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 1024;
const MAX_PATH_DEPTH: usize = 64;
const MAX_ARGUMENT_PREFIX: usize = 32;
const MAX_ARGUMENT_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostOperatingSystem {
    LinuxAmd64,
    LinuxArm64,
    MacosArm64,
    WindowsAmd64,
}

impl HostOperatingSystem {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "linux-amd64",
            Self::LinuxArm64 => "linux-arm64",
            Self::MacosArm64 => "macos-arm64",
            Self::WindowsAmd64 => "windows-amd64",
        }
    }

    #[must_use]
    pub const fn current() -> Option<Self> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            return Some(Self::LinuxAmd64);
        }
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        {
            return Some(Self::LinuxArm64);
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            return Some(Self::MacosArm64);
        }
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        {
            return Some(Self::WindowsAmd64);
        }
        #[allow(unreachable_code)]
        None
    }
}

impl fmt::Display for HostOperatingSystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseIdentity {
    pub id: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseArchive {
    pub format: String,
    pub sha256: ToolDigest,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseFile {
    pub path: String,
    pub sha256: ToolDigest,
    pub size_bytes: u64,
    pub executable: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseExecutable {
    pub path: String,
    pub sha256: ToolDigest,
    pub argument_prefix: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriverReleaseComponentRole {
    Driver,
    OfficialTool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriverReleaseImplementation {
    Builtin,
    Executable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseComponent {
    pub role: DriverReleaseComponentRole,
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_kind: Option<OfficialToolKind>,
    pub implementation: DriverReleaseImplementation,
    pub materialization_digest: ToolDigest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_distribution_digest: Option<ToolDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<DriverReleaseExecutable>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverHostDistribution {
    pub host_operating_system: HostOperatingSystem,
    pub archive: DriverReleaseArchive,
    pub tree_digest: ToolDigest,
    pub components: Vec<DriverReleaseComponent>,
    pub files: Vec<DriverReleaseFile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReleaseInstallationPolicy {
    pub archive_format: String,
    pub allow_links: bool,
    pub allow_non_regular_files: bool,
    pub allow_extra_files: bool,
    pub content_addressed: bool,
    pub read_only_after_install: bool,
    pub verify_before_every_execution: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ProviderDriverRelease {
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    pub schema_version: u32,
    pub driver_contract_version: String,
    pub driver: DriverReleaseIdentity,
    pub distributions: Vec<DriverHostDistribution>,
    pub installation_policy: DriverReleaseInstallationPolicy,
}

#[derive(Clone, Debug)]
pub struct ValidatedProviderDriverRelease(ProviderDriverRelease);

impl ValidatedProviderDriverRelease {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, DriverReleaseError> {
        let release = serde_json::from_slice(bytes).map_err(DriverReleaseError::Json)?;
        Self::new(release)
    }

    pub fn new(release: ProviderDriverRelease) -> Result<Self, DriverReleaseError> {
        validate_release(&release)?;
        Ok(Self(release))
    }

    #[must_use]
    pub fn get(&self) -> &ProviderDriverRelease {
        &self.0
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, DriverReleaseError> {
        serde_jcs::to_vec(&self.0).map_err(DriverReleaseError::Json)
    }

    pub fn canonical_sha256(&self) -> Result<Sha256Digest, DriverReleaseError> {
        Ok(sha256_digest(&self.canonical_json()?))
    }

    #[must_use]
    pub fn distribution_for(&self, host: HostOperatingSystem) -> Option<&DriverHostDistribution> {
        self.0
            .distributions
            .iter()
            .find(|distribution| distribution.host_operating_system == host)
    }

    pub fn bind_to_manifest(
        &self,
        manifest: &ValidatedProviderDriverManifest,
    ) -> Result<(), DriverReleaseError> {
        bind_release_to_manifest(&self.0, manifest.get(), self.canonical_sha256()?)
    }
}

#[derive(Debug)]
pub enum DriverReleaseError {
    Json(serde_json::Error),
    Invalid(&'static str),
}

impl fmt::Display for DriverReleaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "invalid provider driver release JSON: {error}"),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DriverReleaseError {}

fn sha256_digest(bytes: &[u8]) -> Sha256Digest {
    hex::encode(Sha256::digest(bytes))
        .parse()
        .expect("hex encoding a SHA-256 always produces a canonical digest")
}

fn digest_is_sha256(digest: &ToolDigest) -> bool {
    digest.algorithm == "sha256"
}

fn portable_path(path: &str) -> bool {
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || path.contains(['\\', ':', '\0'])
    {
        return false;
    }
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.len() > MAX_PATH_DEPTH {
        return false;
    }
    parts.iter().all(|part| {
        !part.is_empty()
            && *part != "."
            && *part != ".."
            && !part.ends_with('.')
            && part.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'.' | b'_' | b'@' | b'+' | b'~' | b'-')
            })
            && !is_windows_reserved_name(part)
    })
}

fn is_windows_reserved_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).to_ascii_lowercase();
    matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || stem
            .strip_prefix("com")
            .or_else(|| stem.strip_prefix("lpt"))
            .is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

fn validate_release(release: &ProviderDriverRelease) -> Result<(), DriverReleaseError> {
    macro_rules! require {
        ($condition:expr, $message:literal) => {
            if !$condition {
                return Err(DriverReleaseError::Invalid($message));
            }
        };
    }

    require!(
        release
            .schema
            .as_deref()
            .is_some_and(|schema| !schema.is_empty()),
        "provider driver release must identify its JSON schema"
    );
    require!(
        release.schema_version == 1,
        "provider driver release schemaVersion must be 1"
    );
    require!(
        release.driver_contract_version == DRIVER_CONTRACT_VERSION,
        "provider driver release contract version differs from the executable contract"
    );
    require!(
        crate::manifest::is_contract_identifier(&release.driver.id)
            && crate::manifest::is_schema_semver(&release.driver.version),
        "provider driver release identity is invalid"
    );
    require!(
        !release.distributions.is_empty() && release.distributions.len() <= MAX_DISTRIBUTIONS,
        "provider driver release must contain one to four host distributions"
    );
    let hosts = release
        .distributions
        .iter()
        .map(|distribution| distribution.host_operating_system)
        .collect::<Vec<_>>();
    require!(
        hosts.windows(2).all(|pair| pair[0] < pair[1])
            && hosts.iter().collect::<BTreeSet<_>>().len() == hosts.len(),
        "provider driver host distributions must be unique and canonically ordered"
    );
    let policy = &release.installation_policy;
    require!(
        policy.archive_format == "tar"
            && !policy.allow_links
            && !policy.allow_non_regular_files
            && !policy.allow_extra_files
            && policy.content_addressed
            && policy.read_only_after_install
            && policy.verify_before_every_execution,
        "provider driver installation policy may not weaken the v1 trust boundary"
    );

    for distribution in &release.distributions {
        validate_distribution(distribution, &release.driver)?;
    }
    Ok(())
}

fn validate_distribution(
    distribution: &DriverHostDistribution,
    driver: &DriverReleaseIdentity,
) -> Result<(), DriverReleaseError> {
    macro_rules! require {
        ($condition:expr, $message:literal) => {
            if !$condition {
                return Err(DriverReleaseError::Invalid($message));
            }
        };
    }

    require!(
        distribution.archive.format == "tar"
            && digest_is_sha256(&distribution.archive.sha256)
            && (1..=MAX_ARCHIVE_BYTES).contains(&distribution.archive.size_bytes),
        "provider driver archive identity is invalid"
    );
    require!(
        digest_is_sha256(&distribution.tree_digest),
        "provider driver tree digest must use sha256"
    );
    require!(
        !distribution.files.is_empty() && distribution.files.len() <= MAX_FILES,
        "provider driver distribution file count is outside the v1 bound"
    );
    let mut total_bytes = 0_u64;
    let mut previous_path: Option<&str> = None;
    let mut folded_paths = BTreeSet::new();
    for file in &distribution.files {
        require!(
            portable_path(&file.path),
            "provider driver distribution path is not portable"
        );
        require!(
            digest_is_sha256(&file.sha256),
            "provider driver file digest must use sha256"
        );
        require!(
            file.size_bytes <= MAX_FILE_BYTES,
            "provider driver file exceeds the v1 size bound"
        );
        total_bytes =
            total_bytes
                .checked_add(file.size_bytes)
                .ok_or(DriverReleaseError::Invalid(
                    "provider driver distribution size overflowed",
                ))?;
        require!(
            total_bytes <= MAX_ARCHIVE_BYTES,
            "provider driver file tree exceeds the v1 size bound"
        );
        if let Some(previous) = previous_path {
            require!(
                previous < file.path.as_str(),
                "provider driver file table is not canonically ordered"
            );
            require!(
                !file.path.starts_with(&format!("{previous}/")),
                "provider driver file path has a file as its parent"
            );
        }
        require!(
            folded_paths.insert(file.path.to_ascii_lowercase()),
            "provider driver file paths collide on a case-insensitive host"
        );
        previous_path = Some(&file.path);
    }
    let tree_bytes = serde_jcs::to_vec(&distribution.files).map_err(DriverReleaseError::Json)?;
    require!(
        distribution.tree_digest.value == sha256_digest(&tree_bytes),
        "provider driver tree digest does not bind the canonical file table"
    );

    require!(
        !distribution.components.is_empty() && distribution.components.len() <= MAX_COMPONENTS,
        "provider driver distribution component count is outside the v1 bound"
    );
    let file_table = distribution
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut component_keys = BTreeSet::new();
    let mut driver_components = 0;
    for component in &distribution.components {
        require!(
            !component.name.is_empty()
                && component.name.len() <= 200
                && !component.version.is_empty()
                && component.version.len() <= 200
                && digest_is_sha256(&component.materialization_digest),
            "provider driver release component identity is invalid"
        );
        require!(
            component_keys.insert((component.role, component.name.as_str())),
            "provider driver release repeats a component"
        );
        match component.role {
            DriverReleaseComponentRole::Driver => {
                driver_components += 1;
                require!(
                    component.name == driver.id
                        && component.version == driver.version
                        && component.tool_kind.is_none()
                        && component.source_distribution_digest.is_none(),
                    "provider driver component differs from the release identity"
                );
            }
            DriverReleaseComponentRole::OfficialTool => require!(
                component.tool_kind.is_some()
                    && component
                        .source_distribution_digest
                        .as_ref()
                        .is_some_and(digest_is_sha256),
                "official tool component lacks its kind or source distribution digest"
            ),
        }
        match component.implementation {
            DriverReleaseImplementation::Builtin => require!(
                component.file_paths.is_none() && component.executable.is_none(),
                "builtin release component may not identify files or an executable"
            ),
            DriverReleaseImplementation::Executable => {
                let file_paths =
                    component
                        .file_paths
                        .as_ref()
                        .ok_or(DriverReleaseError::Invalid(
                            "executable release component lacks its file set",
                        ))?;
                let executable =
                    component
                        .executable
                        .as_ref()
                        .ok_or(DriverReleaseError::Invalid(
                            "executable release component lacks its entrypoint",
                        ))?;
                require!(
                    !file_paths.is_empty()
                        && file_paths.len() <= MAX_FILES
                        && file_paths.windows(2).all(|pair| pair[0] < pair[1])
                        && file_paths.iter().all(|path| portable_path(path)),
                    "provider driver component file set is not canonical"
                );
                let component_files = file_paths
                    .iter()
                    .map(|path| file_table.get(path.as_str()).copied())
                    .collect::<Option<Vec<_>>>()
                    .ok_or(DriverReleaseError::Invalid(
                        "provider driver component references a file outside the file table",
                    ))?;
                let component_bytes =
                    serde_jcs::to_vec(&component_files).map_err(DriverReleaseError::Json)?;
                require!(
                    portable_path(&executable.path)
                        && digest_is_sha256(&executable.sha256)
                        && executable.argument_prefix.len() <= MAX_ARGUMENT_PREFIX
                        && executable.argument_prefix.iter().all(|argument| {
                            argument.len() <= MAX_ARGUMENT_BYTES && !argument.contains('\0')
                        }),
                    "provider driver executable entrypoint is invalid"
                );
                require!(
                    file_paths.binary_search(&executable.path).is_ok()
                        && component.materialization_digest.value
                            == sha256_digest(&component_bytes)
                        && file_table
                            .get(executable.path.as_str())
                            .is_some_and(|file| {
                                file.executable && file.sha256 == executable.sha256
                            }),
                    "provider driver executable component is not bound to its exact file set"
                );
            }
        }
    }
    require!(
        driver_components == 1,
        "provider driver distribution must contain exactly one driver component"
    );
    Ok(())
}

fn bind_release_to_manifest(
    release: &ProviderDriverRelease,
    manifest: &ProviderDriverManifest,
    release_digest: Sha256Digest,
) -> Result<(), DriverReleaseError> {
    macro_rules! require {
        ($condition:expr, $message:literal) => {
            if !$condition {
                return Err(DriverReleaseError::Invalid($message));
            }
        };
    }

    require!(
        release.driver_contract_version == manifest.driver_contract_version
            && release.driver.id == manifest.driver.id
            && release.driver.version == manifest.driver.version
            && manifest.driver.distribution_digest.algorithm == "sha256"
            && manifest.driver.distribution_digest.value == release_digest,
        "provider driver manifest does not bind this canonical release index"
    );
    let hosts = release
        .distributions
        .iter()
        .map(|distribution| distribution.host_operating_system.as_str())
        .collect::<Vec<_>>();
    require!(
        hosts == manifest.supported_host_operating_systems,
        "provider driver release host set differs from the manifest"
    );
    let expected_tools = manifest
        .official_tools
        .iter()
        .map(|tool| {
            (
                tool.kind,
                tool.name.as_str(),
                tool.version.as_str(),
                &tool.distribution_digest,
            )
        })
        .collect::<BTreeSet<_>>();
    for distribution in &release.distributions {
        let actual_tools = distribution
            .components
            .iter()
            .filter(|component| component.role == DriverReleaseComponentRole::OfficialTool)
            .map(|component| {
                (
                    component.tool_kind.expect("validated official tool kind"),
                    component.name.as_str(),
                    component.version.as_str(),
                    component
                        .source_distribution_digest
                        .as_ref()
                        .expect("validated official tool source digest"),
                )
            })
            .collect::<BTreeSet<_>>();
        require!(
            actual_tools == expected_tools,
            "provider driver host distribution differs from the manifest official tool set"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE_FIXTURE: &[u8] =
        include_bytes!("../../../contracts/v1/fixtures/valid/provider-driver-release.json");
    const MANIFEST_FIXTURE: &[u8] =
        include_bytes!("../../../contracts/v1/fixtures/valid/provider-driver.json");

    fn fixtures() -> (
        ValidatedProviderDriverRelease,
        ValidatedProviderDriverManifest,
    ) {
        (
            ValidatedProviderDriverRelease::parse_json(RELEASE_FIXTURE).unwrap(),
            ValidatedProviderDriverManifest::parse_json(MANIFEST_FIXTURE).unwrap(),
        )
    }

    #[test]
    fn one_release_identity_binds_every_declared_host_distribution() {
        let (release, manifest) = fixtures();
        release.bind_to_manifest(&manifest).unwrap();
        assert_eq!(
            release.canonical_sha256().unwrap(),
            manifest.get().driver.distribution_digest.value
        );
        assert_eq!(release.get().distributions.len(), 4);
        assert!(
            release
                .distribution_for(HostOperatingSystem::WindowsAmd64)
                .is_some()
        );
    }

    #[test]
    fn release_refuses_host_path_and_tree_identity_drift() {
        let release: ProviderDriverRelease = serde_json::from_slice(RELEASE_FIXTURE).unwrap();

        let mut duplicate_host = release.clone();
        duplicate_host.distributions[1].host_operating_system = HostOperatingSystem::LinuxAmd64;
        assert!(ValidatedProviderDriverRelease::new(duplicate_host).is_err());

        let mut traversal = release.clone();
        traversal.distributions[0].files[0].path = "../driver".into();
        assert!(ValidatedProviderDriverRelease::new(traversal).is_err());

        let mut tree_drift = release;
        tree_drift.distributions[0].files[0].size_bytes += 1;
        assert!(ValidatedProviderDriverRelease::new(tree_drift).is_err());
    }

    #[test]
    fn release_refuses_unlisted_or_non_executable_entrypoints() {
        let release: ProviderDriverRelease = serde_json::from_slice(RELEASE_FIXTURE).unwrap();

        let mut missing = release.clone();
        missing.distributions[0].components[0]
            .executable
            .as_mut()
            .unwrap()
            .path = "bin/missing".into();
        assert!(ValidatedProviderDriverRelease::new(missing).is_err());

        let mut not_executable = release;
        not_executable.distributions[0].files[0].executable = false;
        let files = serde_jcs::to_vec(&not_executable.distributions[0].files).unwrap();
        not_executable.distributions[0].tree_digest.value = sha256_digest(&files);
        assert!(ValidatedProviderDriverRelease::new(not_executable).is_err());
    }

    #[test]
    fn manifest_cannot_claim_a_different_release_or_host_set() {
        let (release, manifest) = fixtures();
        let mut changed = manifest.get().clone();
        changed.driver.distribution_digest.value =
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse()
                .unwrap();
        let changed = ValidatedProviderDriverManifest::new(changed).unwrap();
        assert!(release.bind_to_manifest(&changed).is_err());

        let mut changed = manifest.get().clone();
        changed.supported_host_operating_systems.pop();
        let changed = ValidatedProviderDriverManifest::new(changed).unwrap();
        assert!(release.bind_to_manifest(&changed).is_err());
    }
}
