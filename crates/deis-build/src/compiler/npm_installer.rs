//! Locked npm installation for the canonical build graph.
//!
//! npm is an implementation detail here, not an application runtime. The
//! compiler invokes one exact Node/npm toolchain in a scratch-only project,
//! suppresses lifecycle scripts, audits the resulting tree, and then lets the
//! bundler consume it. No provider receives registry credentials or a runtime
//! package manager.

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use deis_build_core::{NodeNpmUsage, NODE_SEMANTIC_BASELINE};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha512};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::time::{sleep, sleep_until, timeout, Instant};
use unicode_normalization::UnicodeNormalization;

const NPM_TOOLCHAIN_VERSION: &str = "11.19.0";
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
const INSTALL_MONITOR_INTERVAL: Duration = Duration::from_millis(50);
pub(super) const MAX_LOCKED_PACKAGES: usize = 10_000;
pub(super) const MAX_INSTALLED_FILES: usize = 100_000;
pub(super) const MAX_DOWNLOADED_OCTETS: u64 = 256 * 1024 * 1024;
pub(super) const MAX_INSTALLED_OCTETS: u64 = 512 * 1024 * 1024;
pub(super) const MAX_EXPANSION_RATIO: u64 = 100;
// npm's content-addressed cache also contains integrity indexes and temporary
// download files. Bound those bytes while allowing fixed bookkeeping outside
// the archive budget that is checked exactly after installation.
const MAX_CACHE_OVERHEAD_OCTETS: u64 = 16 * 1024 * 1024;
const MAX_INSTALL_WORK_ENTRIES: usize = 200_000;
const NODE_EXECUTABLE_ENV: &str = "DEIS_NODE_EXECUTABLE";
const NPM_CLI_ENV: &str = "DEIS_NPM_CLI";
const NPM_USERCONFIG_ENV: &str = "DEIS_NPM_USERCONFIG";

/// The limits enforced by this build implementation, available before a
/// project is read or an npm process starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeNpmBuildLimits {
    pub schema_version: &'static str,
    pub node_semantic_baseline: &'static str,
    pub npm_toolchain_version: &'static str,
    pub max_locked_packages: u64,
    pub max_installed_files: u64,
    pub max_downloaded_octets: u64,
    pub max_download_cache_work_octets: u64,
    pub max_installed_octets: u64,
    pub max_expansion_ratio: u64,
    pub max_install_work_entries: u64,
    pub install_timeout_milliseconds: u64,
    pub monitor_interval_milliseconds: u64,
}

pub const fn node_npm_build_limits() -> NodeNpmBuildLimits {
    NodeNpmBuildLimits {
        schema_version: "edge-canon.node-npm-limits/v1",
        node_semantic_baseline: NODE_SEMANTIC_BASELINE,
        npm_toolchain_version: NPM_TOOLCHAIN_VERSION,
        max_locked_packages: MAX_LOCKED_PACKAGES as u64,
        max_installed_files: MAX_INSTALLED_FILES as u64,
        max_downloaded_octets: MAX_DOWNLOADED_OCTETS,
        max_download_cache_work_octets: MAX_DOWNLOADED_OCTETS + MAX_CACHE_OVERHEAD_OCTETS,
        max_installed_octets: MAX_INSTALLED_OCTETS,
        max_expansion_ratio: MAX_EXPANSION_RATIO,
        max_install_work_entries: MAX_INSTALL_WORK_ENTRIES as u64,
        install_timeout_milliseconds: INSTALL_TIMEOUT.as_millis() as u64,
        monitor_interval_milliseconds: INSTALL_MONITOR_INTERVAL.as_millis() as u64,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NpmInstallEvidence {
    pub node_version: String,
    pub npm_version: String,
    pub package_count: usize,
    pub file_count: usize,
    pub downloaded_octets: u64,
    pub installed_octets: u64,
}

#[derive(Debug, Clone)]
struct NpmToolchain {
    node: PathBuf,
    npm_cli: PathBuf,
}

impl NpmToolchain {
    fn discover() -> anyhow::Result<Self> {
        let node = resolve_executable(NODE_EXECUTABLE_ENV, "node")?;
        let npm_cli = match env::var_os(NPM_CLI_ENV) {
            Some(path) => canonical_regular_file(PathBuf::from(path), NPM_CLI_ENV)?,
            None => discover_npm_cli(&node)?,
        };
        Ok(Self { node, npm_cli })
    }

    async fn verify(&self) -> anyhow::Result<(String, String)> {
        let node_version = version_output(&self.node, &[OsStr::new("--version")]).await?;
        if node_version != format!("v{NODE_SEMANTIC_BASELINE}") {
            anyhow::bail!(
                "EC_NODE_BASELINE_UNSUPPORTED: the configured build Node is {node_version:?}; Edge Canon requires exactly v{NODE_SEMANTIC_BASELINE}. Set {NODE_EXECUTABLE_ENV} to the absolute Node executable"
            );
        }
        let npm_version = version_output(
            &self.node,
            &[self.npm_cli.as_os_str(), OsStr::new("--version")],
        )
        .await?;
        if npm_version != NPM_TOOLCHAIN_VERSION {
            anyhow::bail!(
                "EC_NPM_TOOLCHAIN_UNSUPPORTED: the configured npm CLI is {npm_version:?}; this build is pinned to {NPM_TOOLCHAIN_VERSION}. Set {NPM_CLI_ENV} to the absolute npm-cli.js path"
            );
        }
        Ok((node_version, npm_version))
    }
}

/// Install only when a registry package is actually reachable from source.
pub(super) async fn stage_locked_dependencies(
    source_root: &Path,
    staged_root: &Path,
    usage: &NodeNpmUsage,
) -> anyhow::Result<Option<NpmInstallEvidence>> {
    if usage.registry_packages.is_empty() {
        return Ok(None);
    }
    if usage.locked_package_count > MAX_LOCKED_PACKAGES {
        anyhow::bail!(
            "EC_NPM_RESOURCE_LIMIT_EXCEEDED: lock contains {} packages, above this builder's {MAX_LOCKED_PACKAGES}-package limit",
            usage.locked_package_count
        );
    }

    let toolchain = NpmToolchain::discover()?;
    let (node_version, npm_version) = toolchain.verify().await?;
    fs::create_dir_all(staged_root)?;
    copy_exact_input(
        &source_root.join("package.json"),
        &staged_root.join("package.json"),
    )?;
    copy_exact_input(
        &source_root.join("package-lock.json"),
        &staged_root.join("package-lock.json"),
    )?;

    let work = InstallWorkDir::create(staged_root)?;
    let user_config = prepare_user_config(work.path())?;
    let mut command = Command::new(&toolchain.node);
    command
        .arg(&toolchain.npm_cli)
        .args([
            "ci",
            "--ignore-scripts",
            "--omit=dev",
            "--no-audit",
            "--no-fund",
            "--no-progress",
            "--install-links=false",
            "--install-strategy=hoisted",
            "--loglevel=error",
        ])
        .current_dir(staged_root)
        .env_clear()
        .env("PATH", tool_path(&toolchain.node)?)
        .env("NPM_CONFIG_CACHE", work.path().join("cache"))
        .env("NPM_CONFIG_USERCONFIG", user_config.path())
        .env("NPM_CONFIG_IGNORE_SCRIPTS", "true")
        .env("NPM_CONFIG_UPDATE_NOTIFIER", "false")
        .env("NPM_CONFIG_AUDIT", "false")
        .env("NPM_CONFIG_FUND", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    copy_network_environment(&mut command, user_config.contents())?;

    let mut child = command.spawn().map_err(|error| {
        anyhow::anyhow!(
            "EC_NPM_PACKAGE_UNRESOLVED: failed to start the pinned npm build tool: {error}"
        )
    })?;
    let status = wait_for_bounded_install(
        &mut child,
        staged_root,
        &work.path().join("cache"),
        InstallLimits::production(),
        INSTALL_TIMEOUT,
        INSTALL_MONITOR_INTERVAL,
    )
    .await?;
    if !status.success() {
        anyhow::bail!(
            "EC_NPM_PACKAGE_UNRESOLVED: pinned npm install failed with status {}; stdout/stderr are withheld because registry diagnostics may contain credentials",
            status.code().map_or_else(|| "signal".to_string(), |code| code.to_string())
        );
    }

    let downloaded_octets = audit_download_cache(&work.path().join("cache"), staged_root)?;
    let audit = audit_installed_tree(staged_root, usage)?;
    if downloaded_octets > MAX_DOWNLOADED_OCTETS {
        anyhow::bail!(
            "EC_NPM_RESOURCE_LIMIT_EXCEEDED: locked package archives contain {downloaded_octets} octets, above this builder's {MAX_DOWNLOADED_OCTETS}-octet limit"
        );
    }
    if audit.installed_octets > MAX_INSTALLED_OCTETS {
        anyhow::bail!(
            "EC_NPM_RESOURCE_LIMIT_EXCEEDED: installed packages contain {} octets, above this builder's {MAX_INSTALLED_OCTETS}-octet limit",
            audit.installed_octets
        );
    }
    if downloaded_octets == 0
        || u128::from(audit.installed_octets)
            > u128::from(downloaded_octets) * u128::from(MAX_EXPANSION_RATIO)
    {
        anyhow::bail!(
            "EC_NPM_ARCHIVE_BOMB: installed packages expand to {} octets from {downloaded_octets} archive octets, above the {MAX_EXPANSION_RATIO}:1 limit",
            audit.installed_octets
        );
    }
    Ok(Some(NpmInstallEvidence {
        node_version,
        npm_version,
        package_count: audit.package_count,
        file_count: audit.file_count,
        downloaded_octets,
        installed_octets: audit.installed_octets,
    }))
}

#[derive(Clone, Copy)]
struct InstallLimits {
    cache_octets: u64,
    installed_octets: u64,
    installed_files: usize,
    work_entries: usize,
}

impl InstallLimits {
    const fn production() -> Self {
        Self {
            cache_octets: MAX_DOWNLOADED_OCTETS + MAX_CACHE_OVERHEAD_OCTETS,
            installed_octets: MAX_INSTALLED_OCTETS,
            installed_files: MAX_INSTALLED_FILES,
            work_entries: MAX_INSTALL_WORK_ENTRIES,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ObservedTree {
    entries: usize,
    files: usize,
    octets: u64,
}

/// Wait for npm while independently measuring the two directories it can
/// grow. The post-install integrity and path audit remains authoritative; this
/// observer exists so a hostile archive cannot consume the entire configured
/// allowance (or host disk) before that audit gets a chance to run.
async fn wait_for_bounded_install(
    child: &mut Child,
    staged_root: &Path,
    cache_root: &Path,
    limits: InstallLimits,
    deadline_after: Duration,
    monitor_interval: Duration,
) -> anyhow::Result<ExitStatus> {
    let deadline = Instant::now() + deadline_after;
    loop {
        tokio::select! {
            biased;
            _ = sleep_until(deadline) => {
                terminate_install(child).await;
                anyhow::bail!(
                    "EC_NPM_PACKAGE_UNRESOLVED: pinned npm install exceeded {} seconds and was terminated; no provider artifact was produced",
                    deadline_after.as_secs()
                );
            }
            _ = sleep(monitor_interval) => {
                let staged_root = staged_root.to_path_buf();
                let cache_root = cache_root.to_path_buf();
                let observed = tokio::task::spawn_blocking(move || {
                    enforce_install_limits(&staged_root, &cache_root, limits)
                })
                .await;
                let observed = match observed {
                    Ok(observed) => observed,
                    Err(_) => {
                        terminate_install(child).await;
                        anyhow::bail!(
                            "EC_NPM_RESOURCE_LIMIT_EXCEEDED: npm resource monitor failed"
                        );
                    }
                };
                if let Err(error) = observed {
                    terminate_install(child).await;
                    return Err(error);
                }
            }
            result = child.wait() => return result.map_err(Into::into),
        }
    }
}

async fn terminate_install(child: &mut Child) {
    let _ = child.kill().await;
    // `kill` currently waits on every Tokio platform, but an explicit wait
    // also makes the no-zombie invariant survive that implementation detail.
    let _ = child.wait().await;
}

fn enforce_install_limits(
    staged_root: &Path,
    cache_root: &Path,
    limits: InstallLimits,
) -> anyhow::Result<()> {
    let cache = observe_tree(cache_root, limits.work_entries)?;
    if cache.octets > limits.cache_octets {
        anyhow::bail!(
            "EC_NPM_RESOURCE_LIMIT_EXCEEDED: npm download cache exceeded {} octets during package fetch and was terminated",
            limits.cache_octets
        );
    }

    let installed = observe_tree(&staged_root.join("node_modules"), limits.work_entries)?;
    if installed.files > limits.installed_files {
        anyhow::bail!(
            "EC_NPM_RESOURCE_LIMIT_EXCEEDED: npm produced more than {} installed files and was terminated",
            limits.installed_files
        );
    }
    if installed.octets > limits.installed_octets {
        anyhow::bail!(
            "EC_NPM_RESOURCE_LIMIT_EXCEEDED: npm expanded more than {} installed octets and was terminated",
            limits.installed_octets
        );
    }
    Ok(())
}

fn observe_tree(root: &Path, max_entries: usize) -> anyhow::Result<ObservedTree> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ObservedTree::default());
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("EC_NPM_ARCHIVE_PATH_UNSAFE: npm work root is not a regular directory");
    }

    let mut observed = ObservedTree::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                // npm can atomically move a cache entry while the observer is
                // walking it. A vanished name has consumed no remaining disk.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            observed.entries = observed.entries.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!("EC_NPM_RESOURCE_LIMIT_EXCEEDED: npm entry count overflowed")
            })?;
            if observed.entries > max_entries {
                anyhow::bail!(
                    "EC_NPM_RESOURCE_LIMIT_EXCEEDED: npm work tree exceeded {max_entries} entries and was terminated"
                );
            }
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() {
                anyhow::bail!("EC_NPM_ARCHIVE_PATH_UNSAFE: npm created a special filesystem entry");
            }
            observed.files = observed.files.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!("EC_NPM_RESOURCE_LIMIT_EXCEEDED: npm file count overflowed")
            })?;
            observed.octets = observed.octets.checked_add(metadata.len()).ok_or_else(|| {
                anyhow::anyhow!("EC_NPM_RESOURCE_LIMIT_EXCEEDED: npm byte count overflowed")
            })?;
        }
    }
    Ok(observed)
}

async fn version_output(program: &Path, arguments: &[&OsStr]) -> anyhow::Result<String> {
    let result = timeout(
        Duration::from_secs(10),
        Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("tool version check timed out"))??;
    if !result.status.success() || result.stdout.len() > 128 {
        anyhow::bail!("tool version check failed or returned an invalid response");
    }
    String::from_utf8(result.stdout)
        .map(|value| value.trim().to_string())
        .map_err(Into::into)
}

fn resolve_executable(variable: &str, fallback: &str) -> anyhow::Result<PathBuf> {
    if let Some(path) = env::var_os(variable) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            anyhow::bail!("{variable} must be an absolute path");
        }
        return canonical_regular_file(path, variable);
    }
    let search = env::var_os("PATH").ok_or_else(|| {
        anyhow::anyhow!("EC_NPM_TOOLCHAIN_UNAVAILABLE: PATH is absent and {variable} is unset")
    })?;
    for directory in env::split_paths(&search) {
        #[cfg(windows)]
        let candidates = [
            directory.join(format!("{fallback}.exe")),
            directory.join(fallback),
        ];
        #[cfg(not(windows))]
        let candidates = [directory.join(fallback)];
        for candidate in candidates {
            if candidate.is_file() {
                return canonical_regular_file(candidate, fallback);
            }
        }
    }
    anyhow::bail!(
        "EC_NPM_TOOLCHAIN_UNAVAILABLE: cannot find {fallback}; set {variable} to its absolute path"
    )
}

fn canonical_regular_file(path: PathBuf, description: &str) -> anyhow::Result<PathBuf> {
    let path = fs::canonicalize(&path).map_err(|error| {
        anyhow::anyhow!("EC_NPM_TOOLCHAIN_UNAVAILABLE: cannot resolve {description}: {error}")
    })?;
    if !fs::metadata(&path)?.is_file() {
        anyhow::bail!(
            "EC_NPM_TOOLCHAIN_UNAVAILABLE: {description} is not a regular file: {}",
            path.display()
        );
    }
    Ok(path)
}

fn discover_npm_cli(node: &Path) -> anyhow::Result<PathBuf> {
    let bin = node.parent().ok_or_else(|| {
        anyhow::anyhow!("EC_NPM_TOOLCHAIN_UNAVAILABLE: Node executable has no parent directory")
    })?;
    let root = bin.parent().unwrap_or(bin);
    let mut candidates = vec![
        bin.join("npm"),
        bin.join("npm.cmd"),
        root.join("lib/node_modules/npm/bin/npm-cli.js"),
        root.join("node_modules/npm/bin/npm-cli.js"),
        bin.join("node_modules/npm/bin/npm-cli.js"),
    ];
    candidates.dedup();
    for candidate in candidates {
        if candidate.is_file() {
            let resolved = fs::canonicalize(candidate)?;
            if resolved.extension().and_then(OsStr::to_str) == Some("js") {
                return canonical_regular_file(resolved, "npm CLI");
            }
        }
    }
    anyhow::bail!(
        "EC_NPM_TOOLCHAIN_UNAVAILABLE: cannot find npm-cli.js beside {}; set {NPM_CLI_ENV} to its absolute path",
        node.display()
    )
}

fn tool_path(node: &Path) -> anyhow::Result<OsString> {
    let directory = node
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Node executable has no parent directory"))?;
    env::join_paths([directory]).map_err(Into::into)
}

fn copy_exact_input(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let bytes = fs::read(source)?;
    if destination.exists() && fs::read(destination)? != bytes {
        anyhow::bail!(
            "EC_NPM_LOCK_MISMATCH: staged {} already exists with different bytes",
            destination
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
    }
    if !destination.exists() {
        fs::write(destination, bytes)?;
    }
    Ok(())
}

struct InstallWorkDir {
    path: PathBuf,
}

impl InstallWorkDir {
    fn create(staged_root: &Path) -> anyhow::Result<Self> {
        let path = staged_root.join(".deis-npm-work");
        if path.exists() {
            anyhow::bail!(
                "EC_NPM_CACHE_UNSAFE: temporary npm work path already exists: {}",
                path.display()
            );
        }
        fs::create_dir(&path)?;
        set_private_directory(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstallWorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct PreparedUserConfig {
    path: PathBuf,
    contents: Vec<u8>,
}

impl PreparedUserConfig {
    fn path(&self) -> &Path {
        &self.path
    }

    fn contents(&self) -> &[u8] {
        &self.contents
    }
}

fn prepare_user_config(work: &Path) -> anyhow::Result<PreparedUserConfig> {
    let contents = match env::var_os(NPM_USERCONFIG_ENV) {
        Some(source) => {
            let source = PathBuf::from(source);
            if !source.is_absolute() {
                anyhow::bail!("{NPM_USERCONFIG_ENV} must be an absolute path");
            }
            let metadata = fs::symlink_metadata(&source)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                anyhow::bail!("{NPM_USERCONFIG_ENV} must name a regular non-symlink file");
            }
            fs::read(source)?
        }
        None => Vec::new(),
    };
    let path = work.join("user.npmrc");
    fs::write(&path, &contents)?;
    set_private_file(&path)?;
    Ok(PreparedUserConfig { path, contents })
}

fn copy_network_environment(command: &mut Command, user_config: &[u8]) -> anyhow::Result<()> {
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NODE_EXTRA_CA_CERTS",
    ] {
        if let Some(value) = env::var_os(name) {
            command.env(name, value);
        }
    }
    let text = std::str::from_utf8(user_config)
        .map_err(|_| anyhow::anyhow!("{NPM_USERCONFIG_ENV} must be UTF-8"))?;
    for name in referenced_environment_names(text)? {
        let value = env::var_os(&name).ok_or_else(|| {
            anyhow::anyhow!(
                "EC_NPM_AUTH_REQUIRED: npm user config references {name}, but that environment variable is absent"
            )
        })?;
        command.env(name, value);
    }
    Ok(())
}

fn referenced_environment_names(config: &str) -> anyhow::Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    let mut remaining = config;
    while let Some(start) = remaining.find("${") {
        remaining = &remaining[start + 2..];
        let Some(end) = remaining.find('}') else {
            anyhow::bail!("{NPM_USERCONFIG_ENV} contains an unterminated environment reference");
        };
        let name = &remaining[..end];
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
            || name.as_bytes()[0].is_ascii_digit()
        {
            anyhow::bail!("{NPM_USERCONFIG_ENV} contains an invalid environment variable name");
        }
        names.insert(name.to_string());
        remaining = &remaining[end + 1..];
    }
    Ok(names)
}

/// Collision detection is kept apart from the directory walk because the rule
/// cannot otherwise be proven on every host we build on. A case-insensitive
/// volume — APFS and NTFS by default — cannot even hold two paths that differ
/// only in case, so a test that stages such a tree on disk quietly ends up with
/// one file and stops exercising the rule it claims to cover. Folding the
/// collected relative paths through this value-level accumulator lets the claim
/// be asserted directly, on any filesystem, while the walk keeps using exactly
/// the same code in production.
#[derive(Debug, Default)]
struct PortablePathSet {
    seen: BTreeMap<String, PathBuf>,
}

impl PortablePathSet {
    /// Record one installed path relative to `node_modules`, refusing the tree
    /// as soon as two of them fold onto the same name. Such a tree cannot be
    /// unpacked faithfully on a case-insensitive or Unicode-normalizing host:
    /// the second file silently replaces the first, and the bundle then no
    /// longer carries the contents the lock names.
    fn record(&mut self, relative: &Path) -> anyhow::Result<()> {
        let portable = relative.to_string_lossy().replace('\\', "/");
        let collision_key = portable.nfc().collect::<String>().to_lowercase();
        if let Some(previous) = self.seen.insert(collision_key, relative.to_path_buf()) {
            anyhow::bail!(
                "EC_NPM_ARCHIVE_PATH_COLLISION: installed paths {:?} and {:?} collide across supported filesystems",
                previous,
                relative
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeAudit {
    package_count: usize,
    file_count: usize,
    installed_octets: u64,
}

/// Independently bind every registry package's downloaded tarball bytes to
/// its package-lock SHA-512. npm 11's isolated cacache is part of the pinned
/// build toolchain, so no older cache entry or unverified mirror can satisfy
/// this check.
fn audit_download_cache(cache: &Path, staged_root: &Path) -> anyhow::Result<u64> {
    let lock: Value = serde_json::from_slice(&fs::read(staged_root.join("package-lock.json"))?)?;
    let packages = lock
        .get("packages")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("EC_NPM_LOCK_MISMATCH: lock packages object disappeared"))?;
    let content_root = cache.join("_cacache/content-v2/sha512");
    let canonical_content_root = fs::canonicalize(&content_root).map_err(|_| {
        anyhow::anyhow!(
            "EC_NPM_INTEGRITY_FAILED: pinned npm did not retain its verified SHA-512 content cache"
        )
    })?;
    let mut seen = BTreeSet::new();
    let mut downloaded_octets = 0u64;

    for (path, record) in packages.iter().filter(|(path, _)| !path.is_empty()) {
        // npm deliberately omits an optional dependency when its os/cpu/libc
        // constraints do not match this builder. Such a record still belongs
        // in the portable lock, but there are no installed or downloaded
        // bytes to audit on this host. A missing non-optional record is a
        // broken installation and must not receive the same exemption.
        if !staged_root.join(path).exists() {
            if record
                .get("optional")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            anyhow::bail!("EC_NPM_PACKAGE_UNRESOLVED: locked package {path:?} was not installed");
        }
        let encoded = record
            .get("integrity")
            .and_then(Value::as_str)
            .and_then(|value| value.strip_prefix("sha512-"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "EC_NPM_INTEGRITY_REQUIRED: locked package {path:?} lacks SHA-512 integrity"
                )
            })?;
        let digest = BASE64_STANDARD.decode(encoded).map_err(|_| {
            anyhow::anyhow!(
                "EC_NPM_INTEGRITY_REQUIRED: locked package {path:?} has invalid SHA-512 integrity"
            )
        })?;
        if digest.len() != 64 {
            anyhow::bail!(
                "EC_NPM_INTEGRITY_REQUIRED: locked package {path:?} has invalid SHA-512 integrity"
            );
        }
        let digest_hex = hex::encode(&digest);
        if !seen.insert(digest_hex.clone()) {
            continue;
        }
        let content = content_root
            .join(&digest_hex[..2])
            .join(&digest_hex[2..4])
            .join(&digest_hex[4..]);
        let canonical = fs::canonicalize(&content).map_err(|_| {
            anyhow::anyhow!(
                "EC_NPM_INTEGRITY_FAILED: downloaded bytes for locked package {path:?} are absent"
            )
        })?;
        if !canonical.starts_with(&canonical_content_root) {
            anyhow::bail!(
                "EC_NPM_CACHE_UNSAFE: cached bytes for locked package {path:?} escape the isolated content cache"
            );
        }
        let metadata = fs::symlink_metadata(&content)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            anyhow::bail!(
                "EC_NPM_CACHE_UNSAFE: cached bytes for locked package {path:?} are not a regular file"
            );
        }
        reject_hard_link(&metadata, &content)?;
        downloaded_octets = downloaded_octets
            .checked_add(metadata.len())
            .ok_or_else(|| {
                anyhow::anyhow!("EC_NPM_RESOURCE_LIMIT_EXCEEDED: downloaded byte count overflowed")
            })?;
        if downloaded_octets > MAX_DOWNLOADED_OCTETS {
            anyhow::bail!(
                "EC_NPM_RESOURCE_LIMIT_EXCEEDED: locked package archives exceed this builder's {MAX_DOWNLOADED_OCTETS}-octet limit"
            );
        }
        let mut input = BufReader::new(fs::File::open(&content)?);
        let mut actual = Sha512::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            actual.update(&buffer[..count]);
        }
        let actual = actual.finalize();
        if actual.as_slice() != digest.as_slice() {
            anyhow::bail!(
                "EC_NPM_INTEGRITY_FAILED: downloaded bytes for locked package {path:?} differ from package-lock.json"
            );
        }
    }
    Ok(downloaded_octets)
}

fn audit_installed_tree(staged_root: &Path, usage: &NodeNpmUsage) -> anyhow::Result<TreeAudit> {
    let node_modules = staged_root.join("node_modules");
    let root = fs::canonicalize(&node_modules).map_err(|_| {
        anyhow::anyhow!("EC_NPM_PACKAGE_UNRESOLVED: npm produced no node_modules directory")
    })?;
    let lock: Value = serde_json::from_slice(&fs::read(staged_root.join("package-lock.json"))?)?;
    let locked = lock
        .get("packages")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("EC_NPM_LOCK_MISMATCH: lock packages object disappeared"))?;

    let mut package_count = 0usize;
    for path in locked.keys().filter(|path| !path.is_empty()) {
        let package_root = staged_root.join(path);
        if !package_root.exists() {
            continue;
        }
        let canonical = fs::canonicalize(&package_root)?;
        if !canonical.starts_with(&root) {
            anyhow::bail!(
                "EC_NPM_ARCHIVE_PATH_UNSAFE: installed package {path:?} resolves outside node_modules"
            );
        }
        audit_package_manifest(path, &canonical)?;
        package_count += 1;
        if package_count > MAX_LOCKED_PACKAGES {
            anyhow::bail!(
                "EC_NPM_RESOURCE_LIMIT_EXCEEDED: installed package count exceeds {MAX_LOCKED_PACKAGES}"
            );
        }
    }
    for package in &usage.registry_packages {
        if !staged_root.join(format!("node_modules/{package}")).is_dir() {
            anyhow::bail!(
                "EC_NPM_PACKAGE_UNRESOLVED: imported package {package:?} was not installed"
            );
        }
    }

    let mut portable_paths = PortablePathSet::default();
    let mut file_count = 0usize;
    let mut installed_octets = 0u64;
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            let relative = path.strip_prefix(&root).expect("walk stays under root");
            validate_portable_relative_path(relative)?;
            portable_paths.record(relative)?;

            if metadata.file_type().is_symlink() {
                let target = fs::canonicalize(&path)?;
                if !target.starts_with(&root) {
                    anyhow::bail!(
                        "EC_NPM_ARCHIVE_PATH_UNSAFE: installed symlink {:?} escapes node_modules",
                        relative
                    );
                }
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() {
                anyhow::bail!(
                    "EC_NPM_ARCHIVE_PATH_UNSAFE: installed path {:?} is not a regular file or directory",
                    relative
                );
            }
            reject_hard_link(&metadata, relative)?;
            if path.extension().and_then(OsStr::to_str) == Some("node")
                || path.file_name().and_then(OsStr::to_str) == Some("binding.gyp")
            {
                anyhow::bail!(
                    "EC_NPM_NATIVE_ADDON_UNSUPPORTED: installed package contains {:?}",
                    relative
                );
            }
            file_count += 1;
            if file_count > MAX_INSTALLED_FILES {
                anyhow::bail!(
                    "EC_NPM_RESOURCE_LIMIT_EXCEEDED: installed file count exceeds {MAX_INSTALLED_FILES}"
                );
            }
            installed_octets = installed_octets
                .checked_add(metadata.len())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "EC_NPM_RESOURCE_LIMIT_EXCEEDED: installed byte count overflowed"
                    )
                })?;
            if installed_octets > MAX_INSTALLED_OCTETS {
                anyhow::bail!(
                    "EC_NPM_RESOURCE_LIMIT_EXCEEDED: installed packages exceed {MAX_INSTALLED_OCTETS} octets"
                );
            }
        }
    }

    Ok(TreeAudit {
        package_count,
        file_count,
        installed_octets,
    })
}

fn audit_package_manifest(lock_path: &str, package_root: &Path) -> anyhow::Result<()> {
    let path = package_root.join("package.json");
    let document: Value = serde_json::from_slice(&fs::read(&path).map_err(|_| {
        anyhow::anyhow!(
            "EC_NPM_PACKAGE_UNRESOLVED: installed package {lock_path:?} has no package.json"
        )
    })?)?;
    if let Some(scripts) = document.get("scripts").and_then(Value::as_object) {
        for hook in ["preinstall", "install", "postinstall"] {
            if scripts.contains_key(hook) {
                anyhow::bail!(
                    "EC_NPM_LIFECYCLE_SCRIPT_UNSUPPORTED: installed package {lock_path:?} declares {hook}"
                );
            }
        }
    }
    Ok(())
}

fn validate_portable_relative_path(path: &Path) -> anyhow::Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        anyhow::bail!("EC_NPM_ARCHIVE_PATH_UNSAFE: installed path is not relative");
    }
    for component in path.components() {
        let Component::Normal(name) = component else {
            anyhow::bail!("EC_NPM_ARCHIVE_PATH_UNSAFE: installed path contains traversal");
        };
        let name = name.to_string_lossy();
        let stem = name.split('.').next().unwrap_or_default();
        if matches!(
            stem.to_ascii_lowercase().as_str(),
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
        ) {
            anyhow::bail!(
                "EC_NPM_ARCHIVE_PATH_UNSAFE: installed path component {name:?} is not portable"
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn reject_hard_link(metadata: &fs::Metadata, path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() > 1 {
        anyhow::bail!(
            "EC_NPM_ARCHIVE_PATH_UNSAFE: installed file {:?} is hard-linked",
            path
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_hard_link(_metadata: &fs::Metadata, _path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_npm_limits_are_queryable_as_stable_machine_readable_values() {
        let limits = serde_json::to_value(node_npm_build_limits()).expect("serialize limits");
        assert_eq!(limits["schemaVersion"], "edge-canon.node-npm-limits/v1");
        assert_eq!(limits["nodeSemanticBaseline"], NODE_SEMANTIC_BASELINE);
        assert_eq!(limits["npmToolchainVersion"], NPM_TOOLCHAIN_VERSION);
        assert_eq!(limits["maxLockedPackages"], MAX_LOCKED_PACKAGES);
        assert_eq!(limits["maxInstalledFiles"], MAX_INSTALLED_FILES);
        assert_eq!(limits["maxDownloadedOctets"], MAX_DOWNLOADED_OCTETS);
        assert_eq!(limits["maxInstalledOctets"], MAX_INSTALLED_OCTETS);
        assert_eq!(limits["maxExpansionRatio"], MAX_EXPANSION_RATIO);
        assert_eq!(
            limits["monitorIntervalMilliseconds"],
            INSTALL_MONITOR_INTERVAL.as_millis() as u64
        );
    }

    #[test]
    fn install_observer_counts_regular_bytes_without_following_links() {
        let project = tempfile::tempdir().expect("temp project");
        let root = project.path().join("tree");
        fs::create_dir_all(root.join("nested")).expect("tree");
        fs::write(root.join("one"), b"123").expect("first file");
        fs::write(root.join("nested/two"), b"4567").expect("second file");

        assert_eq!(
            observe_tree(&root, 3).expect("bounded tree"),
            ObservedTree {
                entries: 3,
                files: 2,
                octets: 7,
            }
        );
        let error = observe_tree(&root, 2)
            .expect_err("entry ceiling must stop the walk")
            .to_string();
        assert!(error.contains("EC_NPM_RESOURCE_LIMIT_EXCEEDED"), "{error}");
    }

    #[tokio::test]
    async fn install_observer_terminates_a_process_as_the_tree_crosses_its_limit() {
        let project = tempfile::tempdir().expect("temp project");
        let staged = project.path().join("staged");
        let cache = project.path().join("cache");
        fs::create_dir_all(staged.join("node_modules/pkg")).expect("installed tree");
        fs::create_dir_all(&cache).expect("cache tree");
        fs::write(staged.join("node_modules/pkg/expanded.js"), b"12345").expect("expanded file");

        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 30"]);
            command
        };
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ]);
            command
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("long-running child");
        let error = wait_for_bounded_install(
            &mut child,
            &staged,
            &cache,
            InstallLimits {
                cache_octets: 1024,
                installed_octets: 4,
                installed_files: 10,
                work_entries: 20,
            },
            Duration::from_secs(5),
            Duration::from_millis(5),
        )
        .await
        .expect_err("growth must terminate npm")
        .to_string();

        assert!(error.contains("EC_NPM_RESOURCE_LIMIT_EXCEEDED"), "{error}");
        assert!(
            child.try_wait().expect("query child").is_some(),
            "resource failure must reap the child"
        );
    }

    #[test]
    fn npm_config_only_requests_explicit_well_formed_environment_names() {
        assert_eq!(
            referenced_environment_names(
                "//registry.example/:_authToken=${NPM_TOKEN}\nproxy=${HTTPS_PROXY}\n"
            )
            .expect("valid references"),
            BTreeSet::from(["HTTPS_PROXY".to_string(), "NPM_TOKEN".to_string()])
        );
        for invalid in ["x=${}", "x=${1TOKEN}", "x=${TOKEN-NAME}", "x=${TOKEN"] {
            assert!(referenced_environment_names(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn portable_path_validation_rejects_traversal_and_windows_devices() {
        for invalid in [Path::new("../x"), Path::new("nested/CON.txt")] {
            assert!(validate_portable_relative_path(invalid).is_err());
        }
        assert!(validate_portable_relative_path(Path::new("@scope/pkg/index.js")).is_ok());
    }

    /// A staged package tree, minus any file that would make the audit refuse it.
    /// The audit reads the lock and the manifest as well as the files on disk,
    /// so every refusal test needs all three in place before it can attribute a
    /// failure to the one rule it is exercising.
    fn staged_installed_tree() -> (tempfile::TempDir, NodeNpmUsage) {
        let project = tempfile::tempdir().expect("temp project");
        fs::create_dir_all(project.path().join("node_modules/pkg")).expect("package root");
        fs::write(
            project.path().join("package-lock.json"),
            r#"{"lockfileVersion":3,"packages":{"":{},"node_modules/pkg":{"version":"1.0.0","resolved":"https://registry.example/pkg.tgz","integrity":"sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="}}}"#,
        )
        .expect("lock");
        fs::write(
            project.path().join("node_modules/pkg/package.json"),
            r#"{"name":"pkg","version":"1.0.0"}"#,
        )
        .expect("package manifest");
        let usage = NodeNpmUsage {
            registry_packages: BTreeSet::from(["pkg".to_string()]),
            locked_package_count: 1,
            ..Default::default()
        };
        (project, usage)
    }

    /// The collision rule asserted at the value level, which is the only way to
    /// assert it everywhere: on a case-insensitive volume the two colliding
    /// names cannot coexist on disk at all, so a filesystem-staged tree can
    /// never present the audit with the input this rule exists to refuse.
    #[test]
    fn portable_path_set_rejects_paths_that_differ_only_in_case_or_normalization() {
        let mut paths = PortablePathSet::default();
        paths.record(Path::new("pkg/A.js")).expect("first path");
        let error = paths
            .record(Path::new("pkg/a.js"))
            .expect_err("a case-only difference must fail")
            .to_string();
        assert!(error.contains("EC_NPM_ARCHIVE_PATH_COLLISION"), "{error}");
        assert!(error.contains("A.js") && error.contains("a.js"), "{error}");

        // Decomposed and precomposed spellings of the same accented name are
        // distinct byte sequences that macOS folds together on the way to disk,
        // so they collide for exactly the reason case-only differences do.
        let mut paths = PortablePathSet::default();
        paths
            .record(Path::new("pkg/caf\u{e9}.js"))
            .expect("precomposed path");
        let error = paths
            .record(Path::new("pkg/cafe\u{301}.js"))
            .expect_err("a normalization-only difference must fail")
            .to_string();
        assert!(error.contains("EC_NPM_ARCHIVE_PATH_COLLISION"), "{error}");

        let mut paths = PortablePathSet::default();
        paths.record(Path::new("pkg/index.js")).expect("first path");
        paths
            .record(Path::new("pkg/nested/index.js"))
            .expect("distinct paths must be accepted");
    }

    /// The end-to-end half of the same claim: the audit must carry the refusal
    /// out of the walk and not merely own the rule. Only a case-sensitive volume
    /// can hold the offending tree, so on a case-insensitive one this asserts the
    /// fold that makes the tree unstageable instead — the rule itself stays
    /// covered unconditionally by the value-level test above.
    #[test]
    fn installed_tree_audit_refuses_a_case_colliding_tree_where_one_can_be_staged() {
        let (project, usage) = staged_installed_tree();
        fs::write(project.path().join("node_modules/pkg/A.js"), "a").expect("first file");
        fs::write(project.path().join("node_modules/pkg/a.js"), "b").expect("second file");

        let upper = fs::read(project.path().join("node_modules/pkg/A.js")).expect("read upper");
        if upper == b"a" {
            let error = audit_installed_tree(project.path(), &usage)
                .expect_err("case collision must fail")
                .to_string();
            assert!(error.contains("EC_NPM_ARCHIVE_PATH_COLLISION"), "{error}");
        } else {
            assert_eq!(
                upper, b"b",
                "a case-insensitive volume must have folded the second write onto the first"
            );
            let audit = audit_installed_tree(project.path(), &usage)
                .expect("a folded tree holds no collision to refuse");
            assert_eq!(
                audit.file_count, 2,
                "the folded tree must hold only the manifest and one script"
            );
        }
    }

    #[test]
    fn installed_tree_audit_rejects_lifecycle_hooks_and_native_addons() {
        let (project, usage) = staged_installed_tree();
        let package_json = project.path().join("node_modules/pkg/package.json");
        fs::write(
            &package_json,
            r#"{"name":"pkg","version":"1.0.0","scripts":{"install":"node-gyp rebuild"}}"#,
        )
        .expect("hook manifest");
        let error = audit_installed_tree(project.path(), &usage)
            .expect_err("install hook must fail")
            .to_string();
        assert!(
            error.contains("EC_NPM_LIFECYCLE_SCRIPT_UNSUPPORTED"),
            "{error}"
        );

        fs::write(&package_json, r#"{"name":"pkg","version":"1.0.0"}"#).expect("clean manifest");
        fs::write(project.path().join("node_modules/pkg/addon.node"), "native")
            .expect("native file");
        let error = audit_installed_tree(project.path(), &usage)
            .expect_err("native addon must fail")
            .to_string();
        assert!(error.contains("EC_NPM_NATIVE_ADDON_UNSUPPORTED"), "{error}");
    }

    #[test]
    fn isolated_cache_bytes_are_bound_to_the_lock_sha512() {
        let project = tempfile::tempdir().expect("temp project");
        let cache = project.path().join("cache");
        let staged = project.path().join("staged");
        fs::create_dir_all(staged.join("node_modules/pkg")).expect("installed package");
        let package = b"a deterministic package archive";
        let digest = Sha512::digest(package);
        let encoded = BASE64_STANDARD.encode(digest);
        let digest_hex = hex::encode(digest);
        let content = cache
            .join("_cacache/content-v2/sha512")
            .join(&digest_hex[..2])
            .join(&digest_hex[2..4])
            .join(&digest_hex[4..]);
        fs::create_dir_all(content.parent().expect("content parent")).expect("cache path");
        fs::write(&content, package).expect("cached bytes");
        fs::write(
            staged.join("package-lock.json"),
            format!(
                r#"{{"lockfileVersion":3,"packages":{{"":{{}},"node_modules/pkg":{{"version":"1.0.0","resolved":"https://registry.example/pkg.tgz","integrity":"sha512-{encoded}"}}}}}}"#
            ),
        )
        .expect("lock");

        assert_eq!(
            audit_download_cache(&cache, &staged).expect("matching bytes"),
            package.len() as u64
        );
        fs::write(&content, b"tampered package archive").expect("tamper cache");
        let error = audit_download_cache(&cache, &staged)
            .expect_err("tampering must fail")
            .to_string();
        assert!(error.contains("EC_NPM_INTEGRITY_FAILED"), "{error}");
    }

    #[test]
    fn isolated_cache_allows_only_missing_optional_platform_packages() {
        let project = tempfile::tempdir().expect("temp project");
        let cache = project.path().join("cache");
        let staged = project.path().join("staged");
        let installed = staged.join("node_modules/pkg");
        fs::create_dir_all(&installed).expect("installed package");
        let package = b"the installed package archive";
        let digest = Sha512::digest(package);
        let encoded = BASE64_STANDARD.encode(digest);
        let digest_hex = hex::encode(digest);
        let content = cache
            .join("_cacache/content-v2/sha512")
            .join(&digest_hex[..2])
            .join(&digest_hex[2..4])
            .join(&digest_hex[4..]);
        fs::create_dir_all(content.parent().expect("content parent")).expect("cache path");
        fs::write(&content, package).expect("cached bytes");
        fs::write(
            staged.join("package-lock.json"),
            format!(
                r#"{{"lockfileVersion":3,"packages":{{"":{{}},"node_modules/pkg":{{"version":"1.0.0","resolved":"https://registry.example/pkg.tgz","integrity":"sha512-{encoded}"}},"node_modules/platform-only":{{"version":"1.0.0","resolved":"https://registry.example/platform-only.tgz","integrity":"sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==","optional":true}}}}}}"#
            ),
        )
        .expect("lock");

        assert_eq!(
            audit_download_cache(&cache, &staged).expect("optional package may be absent"),
            package.len() as u64
        );

        let lock = fs::read_to_string(staged.join("package-lock.json"))
            .expect("read lock")
            .replace("\"optional\":true", "\"optional\":false");
        fs::write(staged.join("package-lock.json"), lock).expect("write non-optional lock");
        let error = audit_download_cache(&cache, &staged)
            .expect_err("missing required package must fail")
            .to_string();
        assert!(error.contains("EC_NPM_PACKAGE_UNRESOLVED"), "{error}");
    }
}
