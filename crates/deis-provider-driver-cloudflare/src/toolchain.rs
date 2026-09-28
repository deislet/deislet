//! The pinned wrangler toolchain: a verified npm dependency tree plus a Node.js
//! runtime that can execute it.
//!
//! wrangler ships as a JavaScript package (`bin/wrangler.js`) with runtime
//! dependencies, so "installing wrangler" means installing a dependency tree.
//! The tree is fixed by `toolchain/package-lock.json`, which is embedded in
//! this binary; see `toolchain/README.md` for why the driver installs it
//! itself instead of shelling out to npm.
//!
//! Guarantees:
//!
//! * the lock's wrangler entry must equal the tool-lock pin, or nothing is
//!   installed;
//! * every tarball is checked against its lock `sha512` integrity before a
//!   single byte of it is extracted, and the wrangler tarball is also checked
//!   against the pinned SHA-256 of the whole `.tgz`;
//! * packages resolve only from the pinned registry, no install scripts run,
//!   and tar entries that are links or escape their package directory are
//!   refused;
//! * the finished tree appears atomically (rename) with a manifest of every
//!   file's SHA-256, and each run re-verifies the tree against it.

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};

use crate::wrangler::WranglerError;

/// The toolchain lock compiled into this driver.
pub const EMBEDDED_TOOLCHAIN_LOCK: &str = include_str!("../toolchain/package-lock.json");

/// The largest tarball the installer accepts, and the largest single file it
/// extracts. workerd's platform binary is the biggest member (~100 MiB).
const MAX_TARBALL_BYTES: usize = 256 * 1024 * 1024;

/// The file inside an installed tree that lists every file's digest.
const MANIFEST_FILE: &str = ".deis-toolchain-manifest.json";

/// The pin the installed tree must honour, from the tool lock.
#[derive(Clone, Debug)]
pub struct ToolchainPin {
    pub version: String,
    pub registry: String,
    pub tarball_url: String,
    pub npm_integrity: String,
    pub distribution_sha256: String,
}

impl ToolchainPin {
    /// The pin this driver was built against.
    pub fn wrangler() -> Self {
        Self {
            version: crate::EXPECTED_WRANGLER_VERSION.to_owned(),
            registry: crate::WRANGLER_REGISTRY.to_owned(),
            tarball_url: crate::WRANGLER_TARBALL.to_owned(),
            npm_integrity: crate::EXPECTED_WRANGLER_NPM_INTEGRITY.to_owned(),
            distribution_sha256: crate::EXPECTED_WRANGLER_DISTRIBUTION_SHA256.to_owned(),
        }
    }
}

/// Fetches a tarball by URL. Production uses [`HttpFetcher`]; tests serve
/// fixture bytes.
pub trait TarballFetcher: Send + Sync {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>>;
}

/// Downloads from the npm registry over HTTPS.
pub struct HttpFetcher {
    client: reqwest::Client,
}

impl HttpFetcher {
    pub fn new() -> Result<Self, WranglerError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!(
                "deis-provider-driver-cloudflare/{}",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|error| WranglerError::Download {
                url: String::new(),
                message: format!("building HTTP client: {error}"),
            })?;
        Ok(Self { client })
    }
}

impl TarballFetcher for HttpFetcher {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>> {
        Box::pin(async move {
            let response = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|error| error.to_string())?;
            if !response.status().is_success() {
                return Err(format!("HTTP {}", response.status()));
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_TARBALL_BYTES as u64)
            {
                return Err("tarball exceeds the download bound".to_owned());
            }
            let bytes = response.bytes().await.map_err(|error| error.to_string())?;
            if bytes.len() > MAX_TARBALL_BYTES {
                return Err("tarball exceeds the download bound".to_owned());
            }
            Ok(bytes.to_vec())
        })
    }
}

/// The host's npm platform triple, used to skip optional packages built for
/// other platforms (`os`, `cpu` and `libc` in the lock).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Platform {
    pub os: String,
    pub cpu: String,
    pub libc: Option<String>,
}

impl Platform {
    pub fn host() -> Self {
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        }
        .to_owned();
        let cpu = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            "x86" => "ia32",
            other => other,
        }
        .to_owned();
        let libc = (os == "linux").then(|| {
            let musl = fs::read_dir("/lib")
                .map(|entries| {
                    entries
                        .flatten()
                        .any(|entry| entry.file_name().to_string_lossy().starts_with("ld-musl-"))
                })
                .unwrap_or(false);
            if musl { "musl" } else { "glibc" }.to_owned()
        });
        Self { os, cpu, libc }
    }

    fn matches(&self, entry: &LockPackage) -> bool {
        field_matches(&entry.os, &self.os)
            && field_matches(&entry.cpu, &self.cpu)
            && match &self.libc {
                Some(libc) => field_matches(&entry.libc, libc),
                None => true,
            }
    }
}

/// npm's rule for `os`/`cpu`/`libc` lists: `!x` excludes x; if any positive
/// value is listed, the host must be one of them.
fn field_matches(values: &[String], host: &str) -> bool {
    if values
        .iter()
        .any(|value| value.strip_prefix('!') == Some(host))
    {
        return false;
    }
    let positives = values
        .iter()
        .filter(|value| !value.starts_with('!'))
        .collect::<Vec<_>>();
    positives.is_empty() || positives.iter().any(|value| value.as_str() == host)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackageLock {
    lockfile_version: u32,
    packages: BTreeMap<String, LockPackage>,
}

#[derive(Debug, Default, Deserialize)]
struct LockPackage {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    resolved: Option<String>,
    #[serde(default)]
    integrity: Option<String>,
    #[serde(default)]
    optional: bool,
    #[serde(default)]
    dev: bool,
    #[serde(default)]
    link: bool,
    #[serde(default, rename = "inBundle")]
    in_bundle: bool,
    #[serde(default)]
    os: Vec<String>,
    #[serde(default)]
    cpu: Vec<String>,
    #[serde(default)]
    libc: Vec<String>,
}

/// One installed, verified toolchain tree.
#[derive(Clone, Debug)]
pub struct InstalledToolchain {
    root: PathBuf,
}

impl InstalledToolchain {
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `node_modules/wrangler/bin/wrangler.js` inside the tree.
    pub fn wrangler_entry(&self) -> PathBuf {
        self.root
            .join("node_modules")
            .join("wrangler")
            .join("bin")
            .join("wrangler.js")
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct Manifest {
    lock_sha256: String,
    files: BTreeMap<String, String>,
}

/// Installs and verifies a toolchain tree under `tools_root`.
pub struct Installer<'a> {
    pub tools_root: &'a Path,
    pub lock_json: &'a str,
    pub pin: &'a ToolchainPin,
    pub fetcher: &'a dyn TarballFetcher,
    pub platform: Platform,
}

impl Installer<'_> {
    /// Return the verified tree, installing it first when it is absent.
    pub async fn ensure(&self) -> Result<InstalledToolchain, WranglerError> {
        let lock = self.parse_and_check_lock()?;
        let lock_sha256 = hex::encode(Sha256::digest(self.lock_json.as_bytes()));
        let dir = self.tools_root.join(format!(
            "wrangler-{}-{}",
            self.pin.version,
            &lock_sha256[..16]
        ));

        if dir.exists() {
            verify_tree(&dir, &lock_sha256)?;
            return Ok(InstalledToolchain { root: dir });
        }

        fs::create_dir_all(self.tools_root).map_err(|error| WranglerError::Io {
            context: format!("creating tool directory {}", self.tools_root.display()),
            error,
        })?;
        let staging = tempfile::Builder::new()
            .prefix(".wrangler-install-")
            .tempdir_in(self.tools_root)
            .map_err(|error| WranglerError::Io {
                context: format!(
                    "creating staging directory in {}",
                    self.tools_root.display()
                ),
                error,
            })?;

        for (path, entry) in &lock.packages {
            if path.is_empty() || entry.dev || entry.in_bundle {
                continue;
            }
            if !self.platform.matches(entry) {
                if entry.optional {
                    continue;
                }
                return Err(WranglerError::Integrity {
                    message: format!(
                        "toolchain lock requires {path} ({:?}/{:?}/{:?}), which does not support \
                         this host ({:?})",
                        entry.os, entry.cpu, entry.libc, self.platform
                    ),
                });
            }
            self.install_package(staging.path(), path, entry).await?;
        }

        let files = hash_tree(staging.path())?;
        let manifest = Manifest {
            lock_sha256: lock_sha256.clone(),
            files,
        };
        let manifest_json =
            serde_json::to_vec_pretty(&manifest).map_err(|error| WranglerError::Io {
                context: "serialising toolchain manifest".to_owned(),
                error: io::Error::other(error),
            })?;
        fs::write(staging.path().join(MANIFEST_FILE), manifest_json).map_err(|error| {
            WranglerError::Io {
                context: "writing toolchain manifest".to_owned(),
                error,
            }
        })?;

        let staged = staging.keep();
        if let Err(error) = fs::rename(&staged, &dir) {
            let _ = fs::remove_dir_all(&staged);
            // Another driver process may have finished the same install
            // first; its tree is as good as ours once it verifies.
            if !dir.exists() {
                return Err(WranglerError::Io {
                    context: format!("moving the toolchain into {}", dir.display()),
                    error,
                });
            }
        }
        verify_tree(&dir, &lock_sha256)?;
        Ok(InstalledToolchain { root: dir })
    }

    fn parse_and_check_lock(&self) -> Result<PackageLock, WranglerError> {
        let lock: PackageLock =
            serde_json::from_str(self.lock_json).map_err(|error| WranglerError::Integrity {
                message: format!("toolchain lock is not a valid package-lock.json: {error}"),
            })?;
        if lock.lockfile_version != 3 {
            return Err(WranglerError::Integrity {
                message: format!(
                    "toolchain lock has lockfileVersion {}, expected 3",
                    lock.lockfile_version
                ),
            });
        }
        let wrangler =
            lock.packages
                .get("node_modules/wrangler")
                .ok_or_else(|| WranglerError::Integrity {
                    message: "toolchain lock has no node_modules/wrangler entry".to_owned(),
                })?;
        if wrangler.version.as_deref() != Some(self.pin.version.as_str())
            || wrangler.resolved.as_deref() != Some(self.pin.tarball_url.as_str())
            || wrangler.integrity.as_deref() != Some(self.pin.npm_integrity.as_str())
        {
            return Err(WranglerError::Integrity {
                message: format!(
                    "toolchain lock pins wrangler {:?} from {:?} ({:?}), but the tool lock pins \
                     {} from {} ({})",
                    wrangler.version,
                    wrangler.resolved,
                    wrangler.integrity,
                    self.pin.version,
                    self.pin.tarball_url,
                    self.pin.npm_integrity
                ),
            });
        }
        let registry_prefix = format!("{}/", self.pin.registry.trim_end_matches('/'));
        for (path, entry) in &lock.packages {
            if path.is_empty() || entry.dev || entry.in_bundle {
                continue;
            }
            if entry.link {
                return Err(WranglerError::Integrity {
                    message: format!("toolchain lock entry {path} is a link, which is refused"),
                });
            }
            let resolved = entry.resolved.as_deref().unwrap_or_default();
            if !resolved.starts_with(&registry_prefix) {
                return Err(WranglerError::Integrity {
                    message: format!(
                        "toolchain lock entry {path} resolves from {resolved:?}, outside the \
                         pinned registry {registry_prefix}"
                    ),
                });
            }
            if entry
                .integrity
                .as_deref()
                .is_none_or(|integrity| !integrity.contains("sha512-"))
            {
                return Err(WranglerError::Integrity {
                    message: format!("toolchain lock entry {path} has no sha512 integrity"),
                });
            }
            if !path.starts_with("node_modules/") || !is_safe_relative(Path::new(path)) {
                return Err(WranglerError::Integrity {
                    message: format!("toolchain lock entry {path:?} is not a node_modules path"),
                });
            }
        }
        Ok(lock)
    }

    async fn install_package(
        &self,
        staging: &Path,
        path: &str,
        entry: &LockPackage,
    ) -> Result<(), WranglerError> {
        let url = entry.resolved.as_deref().unwrap_or_default();
        let integrity = entry.integrity.as_deref().unwrap_or_default();
        let bytes = self
            .fetcher
            .fetch(url)
            .await
            .map_err(|message| WranglerError::Download {
                url: url.to_owned(),
                message,
            })?;
        verify_sri(&bytes, integrity).map_err(|message| WranglerError::Integrity {
            message: format!("{path}: {message}"),
        })?;
        if path == "node_modules/wrangler" {
            verify_sha256(&bytes, &self.pin.distribution_sha256).map_err(|message| {
                WranglerError::Integrity {
                    message: format!("{path}: {message}"),
                }
            })?;
        }
        let destination = staging.join(path);
        extract_package(&bytes, &destination).map_err(|message| WranglerError::Integrity {
            message: format!("{path}: {message}"),
        })?;
        mark_bins_executable(&destination)
    }
}

/// Check a buffer against an SRI string, which may list several hashes; the
/// buffer must match every `sha512-` one, and at least one must be present.
pub fn verify_sri(data: &[u8], sri: &str) -> Result<(), String> {
    use base64::Engine;
    let actual = Sha512::digest(data);
    let mut checked = false;
    for token in sri.split_whitespace() {
        let Some(encoded) = token.strip_prefix("sha512-") else {
            continue;
        };
        let expected = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| format!("integrity {token:?} is not valid base64"))?;
        if expected.as_slice() != actual.as_slice() {
            return Err(format!(
                "tarball SHA-512 mismatch: expected {}, got {}",
                hex::encode(&expected),
                hex::encode(actual)
            ));
        }
        checked = true;
    }
    if checked {
        Ok(())
    } else {
        Err(format!("integrity {sri:?} carries no sha512 hash"))
    }
}

/// Check a buffer against a hex SHA-256.
pub fn verify_sha256(data: &[u8], expected_hex: &str) -> Result<(), String> {
    let actual = hex::encode(Sha256::digest(data));
    if actual.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        Err(format!(
            "tarball SHA-256 mismatch: expected {expected_hex}, got {actual}"
        ))
    }
}

fn is_safe_relative(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
}

/// Extract an npm tarball into `destination`, dropping the top-level
/// directory (`package/` for almost every package) the way npm does.
fn extract_package(tarball: &[u8], destination: &Path) -> Result<(), String> {
    let decoder = flate2::read::GzDecoder::new(tarball);
    let mut archive = tar::Archive::new(decoder);
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    let entries = archive
        .entries()
        .map_err(|error| format!("reading tarball: {error}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("reading tarball entry: {error}"))?;
        let entry_type = entry.header().entry_type();
        let raw_path = entry
            .path()
            .map_err(|error| format!("reading entry path: {error}"))?
            .into_owned();
        let relative = raw_path.components().skip(1).collect::<PathBuf>();
        if relative.as_os_str().is_empty() {
            continue;
        }
        if !is_safe_relative(&relative) {
            return Err(format!(
                "tarball entry {} escapes the package directory",
                raw_path.display()
            ));
        }
        let target = destination.join(&relative);
        if entry_type.is_dir() {
            fs::create_dir_all(&target).map_err(|error| error.to_string())?;
            continue;
        }
        if !entry_type.is_file() {
            // npm drops links and special files from package tarballs; so does
            // the driver, rather than follow one out of the tree.
            continue;
        }
        if entry.size() > MAX_TARBALL_BYTES as u64 {
            return Err(format!("tarball entry {} is too large", raw_path.display()));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let executable = entry
            .header()
            .mode()
            .map(|mode| mode & 0o111 != 0)
            .unwrap_or(false);
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut data)
            .map_err(|error| format!("reading {}: {error}", raw_path.display()))?;
        fs::write(&target, &data).map_err(|error| error.to_string())?;
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(&target, fs::Permissions::from_mode(mode))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// npm marks every `bin` target executable at install time; esbuild and
/// workerd rely on it for their platform binaries.
fn mark_bins_executable(package_dir: &Path) -> Result<(), WranglerError> {
    let manifest_path = package_dir.join("package.json");
    let Ok(contents) = fs::read_to_string(&manifest_path) else {
        return Ok(());
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return Ok(());
    };
    let bins: Vec<String> = match &manifest["bin"] {
        serde_json::Value::String(path) => vec![path.clone()],
        serde_json::Value::Object(map) => map
            .values()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    for bin in bins {
        let relative = Path::new(&bin)
            .components()
            .filter(|component| !matches!(component, Component::CurDir))
            .collect::<PathBuf>();
        if !is_safe_relative(&relative) {
            continue;
        }
        let path = package_dir.join(relative);
        if path.is_file() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).map_err(|error| {
                WranglerError::Io {
                    context: format!("marking {} executable", path.display()),
                    error,
                }
            })?;
        }
    }
    Ok(())
}

/// SHA-256 of every regular file under `root`, keyed by `/`-separated
/// relative path, with a `+x` suffix on the digest of executable files so a
/// permission change is a tree change too.
fn hash_tree(root: &Path) -> Result<BTreeMap<String, String>, WranglerError> {
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|error| WranglerError::Io {
            context: format!("reading {}", dir.display()),
            error,
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| WranglerError::Io {
                context: format!("reading {}", dir.display()),
                error,
            })?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| WranglerError::Io {
                context: format!("inspecting {}", path.display()),
                error,
            })?;
            let relative = path
                .strip_prefix(root)
                .expect("walked path is under its root")
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if relative == MANIFEST_FILE {
                continue;
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                let mut hasher = Sha256::new();
                let mut file = fs::File::open(&path).map_err(|error| WranglerError::Io {
                    context: format!("opening {}", path.display()),
                    error,
                })?;
                io::copy(&mut file, &mut hasher).map_err(|error| WranglerError::Io {
                    context: format!("hashing {}", path.display()),
                    error,
                })?;
                let mut digest = hex::encode(hasher.finalize());
                if metadata.permissions().mode() & 0o111 != 0 {
                    digest.push_str("+x");
                }
                files.insert(relative, digest);
            } else {
                files.insert(relative, "non-regular".to_owned());
            }
        }
    }
    Ok(files)
}

/// Refuse a tree whose files differ from its manifest in any way.
fn verify_tree(dir: &Path, lock_sha256: &str) -> Result<(), WranglerError> {
    let manifest_path = dir.join(MANIFEST_FILE);
    let manifest: Manifest = fs::read(&manifest_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| WranglerError::Integrity {
            message: format!(
                "installed toolchain {} has no readable manifest; remove the directory to \
                 reinstall",
                dir.display()
            ),
        })?;
    if manifest.lock_sha256 != lock_sha256 {
        return Err(WranglerError::Integrity {
            message: format!(
                "installed toolchain {} was built from another lock; remove the directory to \
                 reinstall",
                dir.display()
            ),
        });
    }
    let actual = hash_tree(dir)?;
    if actual != manifest.files {
        let changed = actual
            .iter()
            .filter(|(path, digest)| manifest.files.get(*path) != Some(digest))
            .map(|(path, _)| path.as_str())
            .chain(
                manifest
                    .files
                    .keys()
                    .filter(|path| !actual.contains_key(*path))
                    .map(String::as_str),
            )
            .take(5)
            .collect::<Vec<_>>();
        return Err(WranglerError::Integrity {
            message: format!(
                "installed toolchain {} no longer matches its manifest (changed: {}); refusing \
                 to run it. Remove the directory to reinstall",
                dir.display(),
                changed.join(", ")
            ),
        });
    }
    Ok(())
}

/// A located Node.js runtime that satisfies [`crate::MINIMUM_NODE_MAJOR`].
#[derive(Clone, Debug)]
pub struct NodeRuntime {
    path: PathBuf,
    version: String,
}

impl NodeRuntime {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// Find `node`: `explicit` (from `DEIS_PROVIDER_NODE`) when given, or
    /// else the first `node` on `search_path`. Refuses a runtime older than
    /// `minimum_major`.
    pub async fn locate(
        explicit: Option<&Path>,
        search_path: Option<&std::ffi::OsStr>,
        minimum_major: u32,
    ) -> Result<Self, WranglerError> {
        let path = match explicit {
            Some(path) => {
                if !path.is_absolute() {
                    return Err(WranglerError::Node {
                        message: format!(
                            "DEIS_PROVIDER_NODE must be an absolute path, got {}",
                            path.display()
                        ),
                    });
                }
                path.to_path_buf()
            }
            None => search_path
                .into_iter()
                .flat_map(std::env::split_paths)
                .map(|dir| dir.join("node"))
                .find(|candidate| candidate.is_file())
                .ok_or_else(|| WranglerError::Node {
                    message: "no `node` found: set DEIS_PROVIDER_NODE to an absolute path of a \
                              Node.js >= 22 executable, or put one on PATH"
                        .to_owned(),
                })?,
        };
        let output = tokio::process::Command::new(&path)
            .arg("--version")
            .env_clear()
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|error| WranglerError::Node {
                message: format!("running {} --version: {error}", path.display()),
            })?;
        if !output.status.success() {
            return Err(WranglerError::Node {
                message: format!("{} --version exited with {}", path.display(), output.status),
            });
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let major = version
            .strip_prefix('v')
            .and_then(|rest| rest.split('.').next())
            .and_then(|major| major.parse::<u32>().ok())
            .ok_or_else(|| WranglerError::Node {
                message: format!(
                    "{} --version printed {version:?}, not a Node.js version",
                    path.display()
                ),
            })?;
        if major < minimum_major {
            return Err(WranglerError::Node {
                message: format!(
                    "{} is Node.js {version}; wrangler {} needs >= {minimum_major}.0.0",
                    path.display(),
                    crate::EXPECTED_WRANGLER_VERSION
                ),
            });
        }
        Ok(Self { path, version })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negated_platform_fields_follow_npm() {
        assert!(field_matches(&[], "linux"));
        assert!(field_matches(&["linux".into()], "linux"));
        assert!(!field_matches(&["darwin".into()], "linux"));
        assert!(!field_matches(&["!linux".into()], "linux"));
        assert!(field_matches(&["!win32".into()], "linux"));
    }

    #[test]
    fn sri_requires_a_matching_sha512() {
        use base64::Engine;
        let data = b"payload";
        let good = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(data))
        );
        assert!(verify_sri(data, &good).is_ok());
        assert!(verify_sri(b"other", &good).is_err());
        assert!(verify_sri(data, "sha1-abc").is_err());
    }

    #[test]
    fn embedded_lock_matches_the_driver_pin() {
        let lock: PackageLock = serde_json::from_str(EMBEDDED_TOOLCHAIN_LOCK).unwrap();
        let wrangler = &lock.packages["node_modules/wrangler"];
        let pin = ToolchainPin::wrangler();
        assert_eq!(wrangler.version.as_deref(), Some(pin.version.as_str()));
        assert_eq!(wrangler.resolved.as_deref(), Some(pin.tarball_url.as_str()));
        assert_eq!(
            wrangler.integrity.as_deref(),
            Some(pin.npm_integrity.as_str())
        );
    }

    #[test]
    fn driver_pin_matches_the_official_tool_lock() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../provider-drivers/official-tool-lock.json");
        let lock: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let entry = lock["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["tool"] == "wrangler")
            .unwrap();
        let pin = ToolchainPin::wrangler();
        assert_eq!(entry["version"], pin.version.as_str());
        assert_eq!(entry["registry"], pin.registry.as_str());
        assert_eq!(entry["tarball"], pin.tarball_url.as_str());
        assert_eq!(entry["npmIntegrity"], pin.npm_integrity.as_str());
        assert_eq!(
            entry["distributionSha256"],
            pin.distribution_sha256.as_str()
        );
    }
}
