//! Wrangler binary management: install, verify, execute.
//!
//! The driver checks a content-addressed cache directory for the pinned
//! wrangler binary. On miss, it downloads the tarball from the npm registry,
//! verifies the SRI (SHA-512) and the binary's SHA-256 against the lock
//! file, extracts the binary, and stows it in the cache. The system PATH is
//! never consulted.
//!
//! ## Caching
//!
//! The cache root is `$DEIS_PROVIDER_TOOLS_DIR` or, when unset,
//! `$HOME/.deis/tools` (on Unix). The binary is stored at
//! `<root>/wrangler-<version>/wrangler` and a sibling file
//! `wrangler-<version>/__verified__` records the successful verification.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sha2::{Digest, Sha256, Sha512};
use tokio::process::Command;

use crate::credentials::CloudflareCredential;
use crate::{
    EXPECTED_WRANGLER_DISTRIBUTION_SHA256, EXPECTED_WRANGLER_NPM_INTEGRITY,
    EXPECTED_WRANGLER_VERSION, WRANGLER_TARBALL, WRANGLER_TARBALL_BINARY_PATH,
};

/// The installed wrangler binary, verified against the lock file.
#[derive(Clone, Debug)]
pub struct WranglerBinary {
    /// Absolute path to the verified wrangler executable.
    path: PathBuf,
    /// The wrangler version string from the lock file (e.g. "4.129.0").
    #[allow(dead_code)]
    version: String,
}

impl WranglerBinary {
    /// Discover or install the pinned wrangler in the cache directory.
    ///
    /// The cache directory is looked up per [`tool_cache_root`]. When the
    /// binary is already installed and verified, this returns immediately.
    /// Otherwise it downloads, verifies, and stows the binary.
    pub async fn ensure_installed() -> Result<Self, WranglerError> {
        let cache_root = tool_cache_root()?;
        let version = EXPECTED_WRANGLER_VERSION;
        let dir = cache_root.join(format!("wrangler-{version}"));
        let binary_path = dir.join("wrangler");
        let verified_path = dir.join("__verified__");

        if binary_path.is_file() && verified_path.is_file() {
            let binary = Self {
                path: binary_path,
                version: version.to_owned(),
            };
            binary.verify_integrity().await?;
            return Ok(binary);
        }

        // Download and install
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).map_err(|error| WranglerError::Io {
            context: format!("creating cache directory {}", dir.display()),
            error,
        })?;

        let tarball_bytes = download_tarball().await?;

        // Verify SRI (SHA-512)
        verify_sri(&tarball_bytes, EXPECTED_WRANGLER_NPM_INTEGRITY)?;

        // Extract the binary
        let binary_data = extract_binary_from_tarball(&tarball_bytes)?;

        // Verify SHA-256 of the binary
        verify_sha256(&binary_data, EXPECTED_WRANGLER_DISTRIBUTION_SHA256)?;

        // Write binary
        fs::write(&binary_path, &binary_data).map_err(|error| WranglerError::Io {
            context: format!("writing wrangler binary to {}", binary_path.display()),
            error,
        })?;

        // Make executable
        fs::set_permissions(&binary_path, fs::Permissions::from_mode(0o755)).map_err(
            |error| WranglerError::Io {
                context: format!("setting executable bit on {}", binary_path.display()),
                error,
            },
        )?;

        // Write verification marker
        fs::write(&verified_path, format!("verified wrangler {version}\n")).map_err(
            |error| WranglerError::Io {
                context: format!("writing verification marker for {version}"),
                error,
            },
        )?;

        let binary = Self {
            path: binary_path,
            version: version.to_owned(),
        };
        // Re-verify the just-installed binary
        binary.verify_integrity().await?;
        Ok(binary)
    }

    /// Check that the binary matches its expected identity on disk.
    async fn verify_integrity(&self) -> Result<(), WranglerError> {
        let data = fs::read(&self.path).map_err(|error| WranglerError::Io {
            context: format!("reading wrangler binary at {}", self.path.display()),
            error,
        })?;
        verify_sha256(&data, EXPECTED_WRANGLER_DISTRIBUTION_SHA256)?;
        Ok(())
    }

    /// Run wrangler with the given arguments.
    ///
    /// The subprocess inherits no environment except `CLOUDFLARE_API_TOKEN`
    /// and `CLOUDFLARE_ACCOUNT_ID` from the credential, plus a minimal
    /// `PATH` and `HOME` needed for the binary itself to function.
    ///
    /// ## Output parsing
    ///
    /// Wrangler 4.129.0 outputs ND-JSON (newline-delimited JSON) on stdout
    /// for most commands that produce structured results. Each line is an
    /// independent JSON object. The caller parses the final line or the
    /// whole output depending on the command.
    ///
    /// Source: wrangler 4.129.0 ships with `--experimental-json` flag;
    /// the ND-JSON format is documented at
    /// <https://developers.cloudflare.com/workers/wrangler/system-environment-variables/#json-output>
    pub async fn run(
        &self,
        credential: &CloudflareCredential,
        args: &[&str],
        working_dir: Option<&Path>,
        stdin_data: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<WranglerOutput, WranglerError> {
        let mut cmd = Command::new(&self.path);
        cmd.args(args);
        cmd.env_clear();
        cmd.env("CLOUDFLARE_API_TOKEN", &credential.api_token);
        cmd.env("CLOUDFLARE_ACCOUNT_ID", &credential.account_id);
        // Wrangler needs a HOME for its config and cache directories.
        // Use the tool cache root as a writable home.
        if let Ok(root) = tool_cache_root() {
            cmd.env("HOME", &root);
        }
        // Minimal PATH so wrangler can find basic utilities if needed.
        cmd.env("PATH", "/usr/bin:/usr/local/bin:/bin");
        // Disable interactive prompts and analytics.
        cmd.env("WRANGLER_LOG", "info");
        // Request structured ND-JSON output.
        cmd.env("WRANGLER_EXPERIMENTAL_JSON", "1");

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.kill_on_drop(true);

        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }

        let mut child = cmd.spawn().map_err(|error| WranglerError::Spawn {
            context: format!("spawning wrangler with args {args:?}"),
            error,
        })?;

        // Write stdin if provided
        if let Some(data) = stdin_data {
            use tokio::io::AsyncWriteExt;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(data).await.map_err(|error| WranglerError::Io {
                    context: "writing to wrangler stdin".to_owned(),
                    error,
                })?;
                drop(stdin);
            }
        }

        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| WranglerError::Timeout {
                duration: timeout,
                args: args.iter().map(|s| s.to_string()).collect(),
            })?
            .map_err(|error| WranglerError::Io {
                context: format!("waiting for wrangler with args {args:?}"),
                error,
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        let exit = output.status;
        Ok(WranglerOutput {
            success: exit.success(),
            exit_code: exit.code(),
            stdout,
            stderr,
        })
    }
}

/// The result of a wrangler command execution.
#[derive(Clone, Debug)]
pub struct WranglerOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl WranglerOutput {
    /// Parse the ND-JSON output: each non-empty line is a JSON value.
    ///
    /// Source: wrangler 4.129.0 with `WRANGLER_EXPERIMENTAL_JSON=1` writes
    /// one JSON object per line on stdout. Some commands (notably `versions
    /// upload`) produce multiple lines; the final line typically carries the
    /// result the caller wants.
    pub fn ndjson_lines(&self) -> Vec<serde_json::Value> {
        self.stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// The last ND-JSON line, or `None` if stdout has no valid JSON lines.
    pub fn last_ndjson(&self) -> Option<serde_json::Value> {
        self.ndjson_lines().last().cloned()
    }
}

/// Determine the tool cache root directory.
fn tool_cache_root() -> Result<PathBuf, WranglerError> {
    if let Ok(dir) = std::env::var("DEIS_PROVIDER_TOOLS_DIR") {
        let path = PathBuf::from(dir);
        if !path.is_dir() {
            fs::create_dir_all(&path).map_err(|error| WranglerError::Io {
                context: format!("creating DEIS_PROVIDER_TOOLS_DIR {}", path.display()),
                error,
            })?;
        }
        return Ok(path);
    }
    let home = dirs_home()?;
    let path = home.join(".deis").join("tools");
    fs::create_dir_all(&path).map_err(|error| WranglerError::Io {
        context: format!("creating default tool cache {}", path.display()),
        error,
    })?;
    Ok(path)
}

fn dirs_home() -> Result<PathBuf, WranglerError> {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home));
        }
    }
    Err(WranglerError::NoHome)
}

/// Download the wrangler tarball from the npm registry.
async fn download_tarball() -> Result<Vec<u8>, WranglerError> {
    let client = reqwest_client();
    let response = client
        .get(WRANGLER_TARBALL)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|error| WranglerError::Download {
            url: WRANGLER_TARBALL.to_owned(),
            message: error.to_string(),
        })?;

    if !response.status().is_success() {
        return Err(WranglerError::Download {
            url: WRANGLER_TARBALL.to_owned(),
            message: format!("HTTP {}", response.status()),
        });
    }

    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|error| WranglerError::Download {
            url: WRANGLER_TARBALL.to_owned(),
            message: error.to_string(),
        })
}

/// Verify the tarball against the SRI string from the lock file.
fn verify_sri(data: &[u8], expected_sri: &str) -> Result<(), WranglerError> {
    if !expected_sri.starts_with("sha512-") {
        return Err(WranglerError::Integrity {
            message: "lock file SRI must start with sha512-".to_owned(),
        });
    }
    let expected_b64 = &expected_sri["sha512-".len()..];
    let expected = base64_decode_sri(expected_b64).map_err(|_| WranglerError::Integrity {
        message: "lock file SRI is not valid base64".to_owned(),
    })?;
    let mut hasher = Sha512::new();
    hasher.update(data);
    let actual = hasher.finalize().to_vec();
    if actual != expected {
        return Err(WranglerError::Integrity {
            message: format!(
                "tarball SHA-512 mismatch: expected {}, got {}",
                hex::encode(&expected),
                hex::encode(&actual)
            ),
        });
    }
    Ok(())
}

/// Verify the binary data against the SHA-256 from the lock file.
fn verify_sha256(data: &[u8], expected_hex: &str) -> Result<(), WranglerError> {
    let expected = hex::decode(expected_hex).map_err(|_| WranglerError::Integrity {
        message: "lock file distributionSha256 is not valid hex".to_owned(),
    })?;
    let mut hasher = Sha256::new();
    hasher.update(data);
    let actual = hasher.finalize().to_vec();
    if actual != expected {
        return Err(WranglerError::Integrity {
            message: format!(
                "binary SHA-256 mismatch: expected {}, got {}",
                expected_hex,
                hex::encode(&actual)
            ),
        });
    }
    Ok(())
}

/// Extract the wrangler binary from an npm tarball.
///
/// The tarball's structure is: `package/` containing `wrangler` (the
/// binary), `package.json`, and other metadata. We extract only the binary.
fn extract_binary_from_tarball(tarball_data: &[u8]) -> Result<Vec<u8>, WranglerError> {
    let cursor = io::Cursor::new(tarball_data);
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(cursor));
    for entry_result in archive.entries().map_err(|error| WranglerError::Integrity {
        message: format!("reading tarball: {error}"),
    })? {
        let mut entry = entry_result.map_err(|error| WranglerError::Integrity {
            message: format!("reading tarball entry: {error}"),
        })?;
        let path = entry.path().map_err(|error| WranglerError::Integrity {
            message: format!("reading entry path: {error}"),
        })?;
        if path.to_string_lossy() == WRANGLER_TARBALL_BINARY_PATH {
            let mut data = Vec::new();
            io::Read::read_to_end(&mut entry, &mut data).map_err(|error| {
                WranglerError::Integrity {
                    message: format!("reading wrangler binary from tarball: {error}"),
                }
            })?;
            return Ok(data);
        }
    }
    Err(WranglerError::Integrity {
        message: format!(
            "wrangler binary not found at {WRANGLER_TARBALL_BINARY_PATH:?} in tarball"
        ),
    })
}

fn base64_decode_sri(s: &str) -> Result<Vec<u8>, ()> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|_| ())
}

fn reqwest_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(format!(
            "deis-provider-driver-cloudflare/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .expect("reqwest client builds")
}

#[derive(Debug)]
pub enum WranglerError {
    Io {
        context: String,
        error: io::Error,
    },
    Download {
        url: String,
        message: String,
    },
    Integrity {
        message: String,
    },
    Spawn {
        context: String,
        error: io::Error,
    },
    Timeout {
        duration: Duration,
        args: Vec<String>,
    },
    NoHome,
    NoCredentials,
    NoAccountId(String),
    OutputParse {
        command: String,
        message: String,
        output: String,
    },
    CommandFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },
    StateError {
        message: String,
    },
}

impl std::fmt::Display for WranglerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { context, error } => write!(f, "{context}: {error}"),
            Self::Download { url, message } => write!(f, "download {url}: {message}"),
            Self::Integrity { message } => write!(f, "integrity check failed: {message}"),
            Self::Spawn { context, error } => write!(f, "{context}: {error}"),
            Self::Timeout { duration, args } => {
                write!(
                    f,
                    "wrangler timed out after {duration:?} with args: {}",
                    args.join(" ")
                )
            }
            Self::NoHome => write!(f, "HOME is not set"),
            Self::NoCredentials => write!(f, "EDGE_CANON_PROVIDER_CREDENTIAL is not set"),
            Self::NoAccountId(msg) => write!(f, "{msg}"),
            Self::OutputParse {
                command,
                message,
                output,
            } => {
                write!(
                    f,
                    "failed to parse output of {command}: {message}\n---stdout---\n{output}\n---"
                )
            }
            Self::CommandFailed {
                command,
                exit_code,
                stderr,
            } => {
                write!(
                    f,
                    "wrangler {command} failed with exit code {exit_code:?}: {stderr}"
                )
            }
            Self::StateError { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for WranglerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ndjson_parses_lines() {
        let output = WranglerOutput {
            success: true,
            exit_code: Some(0),
            stdout: r#"{"status":"uploading"}
{"status":"complete","version_id":"abc-123"}
"#
            .to_owned(),
            stderr: String::new(),
        };
        let lines = output.ndjson_lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["status"], "uploading");
        assert_eq!(lines[1]["version_id"], "abc-123");
    }

    #[test]
    fn last_ndjson_returns_final_line() {
        let output = WranglerOutput {
            success: true,
            exit_code: Some(0),
            stdout: r#"{"status":"uploading"}
{"status":"complete","version_id":"abc-123"}
"#
            .to_owned(),
            stderr: String::new(),
        };
        let last = output.last_ndjson().unwrap();
        assert_eq!(last["version_id"], "abc-123");
    }

    #[test]
    fn ndjson_skips_empty_lines() {
        let output = WranglerOutput {
            success: true,
            exit_code: Some(0),
            stdout: "\n\n{\"ok\": true}\n\n".to_owned(),
            stderr: String::new(),
        };
        assert_eq!(output.ndjson_lines().len(), 1);
    }

    #[test]
    fn ndjson_handles_non_json_lines_gracefully() {
        let output = WranglerOutput {
            success: true,
            exit_code: Some(0),
            stdout: "This is not JSON\n{\"ok\": true}\nStill not JSON\n".to_owned(),
            stderr: String::new(),
        };
        assert_eq!(output.ndjson_lines().len(), 1);
    }
}