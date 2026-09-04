//! Edge Canon Node/npm source and lock validation.
//!
//! This module owns the application-facing allow-list. Provider adapters may
//! choose different implementations, but they all receive the same validated
//! source graph and therefore cannot silently grow a provider-specific API.

use crate::ModuleReference;
pub use deis_node_contract::{
    builtin_exports, canonical_builtin, is_known_node_builtin, NODE_BUILTIN_MODULES,
    NODE_SEMANTIC_BASELINE,
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeNpmError {
    pub code: &'static str,
    pub file: PathBuf,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub detail: String,
}

impl NodeNpmError {
    fn source(
        code: &'static str,
        file: impl Into<PathBuf>,
        reference: &ModuleReference,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            code,
            file: file.into(),
            line: Some(reference.line),
            column: Some(reference.column),
            detail: detail.into(),
        }
    }

    fn project(code: &'static str, file: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Self {
            code,
            file: file.into(),
            line: None,
            column: None,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for NodeNpmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.file.display())?;
        if let Some(line) = self.line {
            write!(formatter, ":{line}")?;
            if let Some(column) = self.column {
                write!(formatter, ":{column}")?;
            }
        }
        write!(formatter, ": {}", self.detail)
    }
}

impl std::error::Error for NodeNpmError {}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NodeNpmUsage {
    pub builtin_modules: BTreeSet<String>,
    pub registry_packages: BTreeSet<String>,
    pub locked_package_count: usize,
}

impl NodeNpmUsage {
    pub fn is_used(&self) -> bool {
        !self.builtin_modules.is_empty() || !self.registry_packages.is_empty()
    }
}

/// Validate every runtime module edge and, when npm packages are used, the
/// root manifest/lock pair. Nothing here downloads or executes package code.
pub fn validate_node_npm_project(
    project_root: &Path,
    modules: &[(String, Vec<ModuleReference>)],
) -> Result<NodeNpmUsage, NodeNpmError> {
    let mut usage = NodeNpmUsage::default();
    let mut package_origins: BTreeMap<String, (String, ModuleReference)> = BTreeMap::new();

    for (file, references) in modules {
        for reference in references {
            if let Some(canonical) = canonical_builtin(&reference.specifier) {
                validate_builtin_reference(file, reference, canonical)?;
                usage.builtin_modules.insert(canonical.to_string());
                continue;
            }

            if reference.specifier.starts_with("node:")
                || is_known_node_builtin(&reference.specifier)
            {
                return Err(NodeNpmError::source(
                    "EC_NODE_BUILTIN_UNSUPPORTED",
                    file,
                    reference,
                    format!(
                        "{} is not in the Edge Canon Node {} builtin allow-list",
                        reference.specifier, NODE_SEMANTIC_BASELINE
                    ),
                ));
            }

            if let Some(package) = registry_package_name(&reference.specifier) {
                usage.registry_packages.insert(package.to_string());
                package_origins
                    .entry(package.to_string())
                    .or_insert_with(|| (file.clone(), reference.clone()));
            } else if reference.specifier.starts_with("npm:") {
                return Err(NodeNpmError::source(
                    "EC_NPM_PACKAGE_UNRESOLVED",
                    file,
                    reference,
                    "npm: is a Deno-specific runtime resolver; declare the dependency in package.json and import its bare package name",
                ));
            }
        }
    }

    if !usage.registry_packages.is_empty() {
        usage.locked_package_count = validate_manifest_and_lock(project_root, &package_origins)?;
    }

    Ok(usage)
}

fn validate_builtin_reference(
    file: &str,
    reference: &ModuleReference,
    canonical: &'static str,
) -> Result<(), NodeNpmError> {
    let allowed = builtin_exports(canonical).expect("canonical builtin has an export allow-list");
    if reference.loads_namespace {
        return Err(NodeNpmError::source(
            "EC_NODE_EXPORT_UNSUPPORTED",
            file,
            reference,
            format!(
                "{} uses namespace-producing syntax for {canonical}; import only explicitly allowed exports ({})",
                reference.specifier,
                allowed.join(", ")
            ),
        ));
    }

    for requested in &reference.requested_exports {
        if !allowed.contains(&requested.as_str()) {
            return Err(NodeNpmError::source(
                "EC_NODE_EXPORT_UNSUPPORTED",
                file,
                reference,
                format!(
                    "{canonical} does not expose {requested:?} in the Edge Canon Node {} surface",
                    NODE_SEMANTIC_BASELINE
                ),
            ));
        }
    }
    Ok(())
}

fn validate_manifest_and_lock(
    project_root: &Path,
    imported_packages: &BTreeMap<String, (String, ModuleReference)>,
) -> Result<usize, NodeNpmError> {
    let manifest_path = project_root.join("package.json");
    let lock_path = project_root.join("package-lock.json");
    let manifest = read_json_required(
        &manifest_path,
        "EC_NPM_PACKAGE_UNRESOLVED",
        "npm packages are imported but package.json is missing or invalid",
    )
    .map_err(|error| {
        let (name, (file, reference)) = imported_packages
            .first_key_value()
            .expect("npm validation has at least one imported package");
        NodeNpmError::source(
            error.code,
            file,
            reference,
            format!(
                "package {name:?} is imported but the root package.json is missing or invalid; accepting it would produce an artifact that would fail to load the whole application"
            ),
        )
    })?;
    let lock = read_json_required(
        &lock_path,
        "EC_NPM_LOCK_REQUIRED",
        "npm packages are imported but package-lock.json is missing or invalid",
    )?;

    if lock.get("lockfileVersion").and_then(Value::as_u64) != Some(3) {
        return Err(NodeNpmError::project(
            "EC_NPM_LOCK_VERSION_UNSUPPORTED",
            &lock_path,
            "lockfileVersion must be exactly 3",
        ));
    }

    let packages = lock
        .get("packages")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            NodeNpmError::project(
                "EC_NPM_LOCK_MISMATCH",
                &lock_path,
                "the v3 lock has no packages object",
            )
        })?;
    let root_lock = packages.get("").and_then(Value::as_object).ok_or_else(|| {
        NodeNpmError::project(
            "EC_NPM_LOCK_MISMATCH",
            &lock_path,
            "the v3 lock has no root packages entry",
        )
    })?;

    let manifest_object = manifest.as_object().ok_or_else(|| {
        NodeNpmError::project(
            "EC_NPM_LOCK_MISMATCH",
            &manifest_path,
            "package.json must contain an object",
        )
    })?;
    let declared = dependency_declarations(manifest_object, &manifest_path)?;
    let locked_declarations = dependency_declarations(root_lock, &lock_path)?;

    for (name, requested) in &declared {
        match locked_declarations.get(name) {
            Some(locked) if locked == requested => {}
            _ => {
                return Err(NodeNpmError::project(
                    "EC_NPM_LOCK_MISMATCH",
                    &lock_path,
                    format!(
                        "the root lock declaration for {name:?} does not exactly match package.json"
                    ),
                ));
            }
        }
    }
    for name in locked_declarations.keys() {
        if !declared.contains_key(name) {
            return Err(NodeNpmError::project(
                "EC_NPM_LOCK_MISMATCH",
                &lock_path,
                format!("the root lock declares {name:?}, which is absent from package.json"),
            ));
        }
    }
    for name in imported_packages.keys() {
        if !declared.contains_key(name) {
            return Err(NodeNpmError::project(
                "EC_NPM_PACKAGE_UNRESOLVED",
                &manifest_path,
                format!("imported package {name:?} is not declared by the root package"),
            ));
        }
        if !packages.contains_key(&format!("node_modules/{name}")) {
            return Err(NodeNpmError::project(
                "EC_NPM_PACKAGE_UNRESOLVED",
                &lock_path,
                format!("imported package {name:?} has no root node_modules lock entry"),
            ));
        }
    }

    let mut package_count = 0usize;
    for (path, record) in packages {
        if path.is_empty() {
            continue;
        }
        let record = record.as_object().ok_or_else(|| {
            NodeNpmError::project(
                "EC_NPM_PACKAGE_UNRESOLVED",
                &lock_path,
                format!("lock package entry {path:?} is not an object"),
            )
        })?;
        validate_locked_package(path, record, &lock_path)?;
        package_count += 1;
    }

    Ok(package_count)
}

fn read_json_required(
    path: &Path,
    code: &'static str,
    detail: &'static str,
) -> Result<Value, NodeNpmError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| NodeNpmError::project(code, path, detail))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(NodeNpmError::project(code, path, detail));
    }
    let bytes = fs::read(path).map_err(|_| NodeNpmError::project(code, path, detail))?;
    serde_json::from_slice(&bytes).map_err(|_| NodeNpmError::project(code, path, detail))
}

fn dependency_declarations(
    package: &Map<String, Value>,
    source: &Path,
) -> Result<BTreeMap<String, String>, NodeNpmError> {
    let mut result = BTreeMap::new();
    for field in ["dependencies", "optionalDependencies", "peerDependencies"] {
        let Some(value) = package.get(field) else {
            continue;
        };
        let entries = value.as_object().ok_or_else(|| {
            NodeNpmError::project(
                "EC_NPM_LOCK_MISMATCH",
                source,
                format!("{field} must be an object of package names to version requirements"),
            )
        })?;
        for (name, version) in entries {
            let version = version.as_str().ok_or_else(|| {
                NodeNpmError::project(
                    "EC_NPM_LOCK_MISMATCH",
                    source,
                    format!("the dependency requirement for {name:?} must be text"),
                )
            })?;
            if let Some(previous) = result.insert(name.clone(), version.to_string()) {
                if previous != version {
                    return Err(NodeNpmError::project(
                        "EC_NPM_LOCK_MISMATCH",
                        source,
                        format!("{name:?} has conflicting dependency requirements"),
                    ));
                }
            }
        }
    }
    Ok(result)
}

fn validate_locked_package(
    path: &str,
    record: &Map<String, Value>,
    lock_path: &Path,
) -> Result<(), NodeNpmError> {
    if !path.split('/').any(|part| part == "node_modules")
        || path.starts_with('/')
        || path.starts_with('\\')
        || path
            .split(['/', '\\'])
            .any(|part| part == ".." || part.is_empty())
    {
        return Err(NodeNpmError::project(
            "EC_NPM_ARCHIVE_PATH_UNSAFE",
            lock_path,
            format!("lock package path {path:?} is not a canonical node_modules path"),
        ));
    }
    if record.get("link").and_then(Value::as_bool) == Some(true) {
        return Err(NodeNpmError::project(
            "EC_NPM_PACKAGE_UNRESOLVED",
            lock_path,
            format!(
                "lock package {path:?} is a workspace/file link, not a locked registry package"
            ),
        ));
    }
    if record.get("version").and_then(Value::as_str).is_none()
        || record.get("resolved").and_then(Value::as_str).is_none()
    {
        return Err(NodeNpmError::project(
            "EC_NPM_PACKAGE_UNRESOLVED",
            lock_path,
            format!("lock package {path:?} lacks an exact version or resolved source"),
        ));
    }

    let resolved = record["resolved"].as_str().expect("checked above");
    if !is_credential_free_https_url(resolved) {
        return Err(NodeNpmError::project(
            "EC_NPM_PACKAGE_UNRESOLVED",
            lock_path,
            format!(
                "lock package {path:?} must use a credential-free HTTPS resolved source; the URL is withheld"
            ),
        ));
    }
    let Some(integrity) = record.get("integrity").and_then(Value::as_str) else {
        return Err(NodeNpmError::project(
            "EC_NPM_INTEGRITY_REQUIRED",
            lock_path,
            format!("lock package {path:?} lacks sha512 integrity"),
        ));
    };
    if !valid_sha512_integrity(integrity) {
        return Err(NodeNpmError::project(
            "EC_NPM_INTEGRITY_REQUIRED",
            lock_path,
            format!("lock package {path:?} does not carry a valid sha512 integrity value"),
        ));
    }
    if record.get("hasInstallScript").and_then(Value::as_bool) == Some(true) {
        return Err(NodeNpmError::project(
            "EC_NPM_LIFECYCLE_SCRIPT_UNSUPPORTED",
            lock_path,
            format!("lock package {path:?} declares an install lifecycle hook"),
        ));
    }
    Ok(())
}

fn is_credential_free_https_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    !authority.is_empty()
        && !authority.contains('@')
        && !authority.contains('\\')
        && !authority.chars().any(char::is_whitespace)
}

fn valid_sha512_integrity(value: &str) -> bool {
    let Some(encoded) = value.strip_prefix("sha512-") else {
        return false;
    };
    encoded.len() == 88
        && encoded.ends_with("==")
        && encoded.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '/' | '=')
        })
}

fn registry_package_name(specifier: &str) -> Option<&str> {
    if specifier.is_empty()
        || specifier.starts_with('.')
        || specifier.starts_with('/')
        || specifier.starts_with('#')
        || specifier.contains(':')
    {
        return None;
    }
    if let Some(scoped) = specifier.strip_prefix('@') {
        let slash = scoped.find('/')? + 1;
        let second = specifier[slash + 1..].find('/');
        return Some(match second {
            Some(offset) => &specifier[..slash + 1 + offset],
            None => specifier,
        });
    }
    Some(specifier.split('/').next().expect("non-empty"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModuleReferenceKind;

    fn reference(specifier: &str, exports: &[&str], namespace: bool) -> ModuleReference {
        ModuleReference {
            specifier: specifier.to_string(),
            requested_exports: exports.iter().map(|value| (*value).to_string()).collect(),
            loads_namespace: namespace,
            kind: ModuleReferenceKind::StaticImport,
            line: 3,
            column: 7,
        }
    }

    fn module(reference: ModuleReference) -> Vec<(String, Vec<ModuleReference>)> {
        vec![("functions/index.ts".to_string(), vec![reference])]
    }

    fn integrity() -> &'static str {
        "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="
    }

    #[test]
    fn named_builtin_exports_are_exact_and_bare_aliases_are_canonicalized() {
        let project = tempfile::tempdir().expect("temp project");
        let usage = validate_node_npm_project(
            project.path(),
            &module(reference("crypto", &["createHash", "randomUUID"], false)),
        )
        .expect("selected exports pass");
        assert_eq!(
            usage.builtin_modules,
            BTreeSet::from(["node:crypto".to_string()])
        );
        assert!(usage.registry_packages.is_empty());
    }

    #[test]
    fn unsupported_builtin_export_and_namespace_have_stable_source_diagnostics() {
        let project = tempfile::tempdir().expect("temp project");
        for reference in [
            reference("node:crypto", &["scrypt"], false),
            reference("node:crypto", &[], true),
        ] {
            let error = validate_node_npm_project(project.path(), &module(reference))
                .expect_err("surface expansion must fail");
            assert_eq!(error.code, "EC_NODE_EXPORT_UNSUPPORTED");
            assert_eq!(error.line, Some(3));
            assert_eq!(error.column, Some(7));
            assert!(error.to_string().contains("functions/index.ts:3:7"));
        }
    }

    #[test]
    fn unsupported_node_colon_and_bare_builtins_do_not_become_packages() {
        let project = tempfile::tempdir().expect("temp project");
        for specifier in ["node:fs", "fs", "node:stream/web", "child_process"] {
            let reference = reference(specifier, &[], false);
            let modules = module(reference.clone());
            let error = validate_node_npm_project(project.path(), &modules)
                .expect_err("unsupported builtin must fail");
            assert_eq!(error.code, "EC_NODE_BUILTIN_UNSUPPORTED");
        }
    }

    #[test]
    fn npm_requires_an_exact_v3_manifest_lock_pair() {
        let project = tempfile::tempdir().expect("temp project");
        fs::write(
            project.path().join("package.json"),
            r#"{"name":"app","dependencies":{"pkg":"^1.0.0"}}"#,
        )
        .expect("manifest");
        fs::write(
            project.path().join("package-lock.json"),
            format!(
                r#"{{"name":"app","lockfileVersion":3,"packages":{{"":{{"dependencies":{{"pkg":"^1.0.0"}}}},"node_modules/pkg":{{"version":"1.2.3","resolved":"https://registry.example/pkg/-/pkg-1.2.3.tgz","integrity":"{}"}}}}}}"#,
                integrity()
            ),
        )
        .expect("lock");

        let usage = validate_node_npm_project(
            project.path(),
            &module(reference("pkg/subpath", &["value"], false)),
        )
        .expect("locked package passes preflight");
        assert_eq!(usage.registry_packages, BTreeSet::from(["pkg".to_string()]));
        assert_eq!(usage.locked_package_count, 1);
    }

    #[test]
    fn npm_lock_failures_are_stable_and_never_echo_registry_credentials() {
        let project = tempfile::tempdir().expect("temp project");
        fs::write(
            project.path().join("package.json"),
            r#"{"dependencies":{"@scope/pkg":"1.0.0"}}"#,
        )
        .expect("manifest");
        let secret = "registry-secret";
        fs::write(
            project.path().join("package-lock.json"),
            format!(
                r#"{{"lockfileVersion":3,"packages":{{"":{{"dependencies":{{"@scope/pkg":"1.0.0"}}}},"node_modules/@scope/pkg":{{"version":"1.0.0","resolved":"https://token:{secret}@registry.example/pkg.tgz","integrity":"{}"}}}}}}"#,
                integrity()
            ),
        )
        .expect("lock");

        let error = validate_node_npm_project(
            project.path(),
            &module(reference("@scope/pkg/subpath", &["value"], false)),
        )
        .expect_err("credential-bearing lock URL must fail");
        assert_eq!(error.code, "EC_NPM_PACKAGE_UNRESOLVED");
        assert!(!error.to_string().contains(secret));
        assert!(!error.to_string().contains("token:"));
    }

    #[test]
    fn install_hooks_and_integrity_are_rejected_before_package_execution() {
        let project = tempfile::tempdir().expect("temp project");
        fs::write(
            project.path().join("package.json"),
            r#"{"dependencies":{"pkg":"1.0.0"}}"#,
        )
        .expect("manifest");
        fs::write(
            project.path().join("package-lock.json"),
            r#"{"lockfileVersion":3,"packages":{"":{"dependencies":{"pkg":"1.0.0"}},"node_modules/pkg":{"version":"1.0.0","resolved":"https://registry.example/pkg.tgz","integrity":"sha1-bad","hasInstallScript":true}}}"#,
        )
        .expect("lock");
        let error =
            validate_node_npm_project(project.path(), &module(reference("pkg", &["value"], false)))
                .expect_err("bad integrity fails first");
        assert_eq!(error.code, "EC_NPM_INTEGRITY_REQUIRED");

        let lock = fs::read_to_string(project.path().join("package-lock.json")).expect("lock");
        fs::write(
            project.path().join("package-lock.json"),
            lock.replace("sha1-bad", integrity()),
        )
        .expect("rewrite fixture");
        let error =
            validate_node_npm_project(project.path(), &module(reference("pkg", &["value"], false)))
                .expect_err("install hook fails");
        assert_eq!(error.code, "EC_NPM_LIFECYCLE_SCRIPT_UNSUPPORTED");
    }
}
