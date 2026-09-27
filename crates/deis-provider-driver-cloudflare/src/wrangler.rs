//! Running wrangler: environment, exit codes, and machine-readable output.
//!
//! Every invocation:
//!
//! * starts from an empty environment and receives only the Cloudflare
//!   credential, a private `HOME`, a minimal `PATH`, non-interactive and
//!   no-telemetry switches, and `WRANGLER_OUTPUT_FILE_PATH`;
//! * runs in a working directory the caller chooses (an empty scratch
//!   directory unless the command needs the staged artifact), so wrangler
//!   never picks up an unrelated `wrangler.toml`;
//! * fails with [`WranglerError::CommandFailed`] when wrangler exits non-zero,
//!   carrying the Cloudflare API error code when wrangler reported one;
//! * returns the ND-JSON records wrangler wrote to its output file
//!   (`version-upload`, `version-deploy`, `command-failed`, ...), which is
//!   wrangler 4's documented machine-readable channel.
//!
//! Captured stdout/stderr are truncated and scrubbed of the API token before
//! they are put into an error, so a failure message can be logged safely.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::credentials::CloudflareCredential;
use crate::toolchain::{InstalledToolchain, NodeRuntime};

/// How much of stdout/stderr an error message keeps (the tail, where
/// wrangler prints the error).
const OUTPUT_TAIL_BYTES: usize = 4096;

/// How to start wrangler.
#[derive(Clone, Debug)]
pub struct Wrangler {
    program: PathBuf,
    prefix_args: Vec<OsString>,
    path_env: OsString,
    home: PathBuf,
}

impl Wrangler {
    /// The verified toolchain run by a located Node.js:
    /// `<node> <tree>/node_modules/wrangler/bin/wrangler.js <args>`.
    pub fn installed(node: &NodeRuntime, toolchain: &InstalledToolchain, home: PathBuf) -> Self {
        let mut path_env = OsString::new();
        if let Some(dir) = node.path().parent() {
            path_env.push(dir.as_os_str());
            path_env.push(":");
        }
        path_env.push("/usr/local/bin:/usr/bin:/bin");
        Self {
            program: node.path().to_path_buf(),
            prefix_args: vec![toolchain.wrangler_entry().into_os_string()],
            path_env,
            home,
        }
    }

    /// An executable that stands in for wrangler and takes wrangler's
    /// arguments directly. The driver binary never uses this; it exists so
    /// tests can put a recording fake behind the same runner.
    pub fn from_program(program: PathBuf, home: PathBuf) -> Self {
        Self {
            program,
            prefix_args: Vec::new(),
            path_env: OsString::from("/usr/local/bin:/usr/bin:/bin"),
            home,
        }
    }

    /// Run wrangler with `args` in `working_dir`, feeding `stdin_data` to its
    /// stdin when given (the only channel secret values ever take).
    ///
    /// A non-zero exit is an error.
    pub async fn run(
        &self,
        credential: &CloudflareCredential,
        args: &[&str],
        working_dir: &Path,
        stdin_data: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<WranglerOutput, WranglerError> {
        let command_text = args.join(" ");
        std::fs::create_dir_all(&self.home).map_err(|error| WranglerError::Io {
            context: format!("creating wrangler home {}", self.home.display()),
            error,
        })?;
        let scratch = tempfile::Builder::new()
            .prefix("wrangler-run-")
            .tempdir_in(&self.home)
            .map_err(|error| WranglerError::Io {
                context: "creating wrangler output directory".to_owned(),
                error,
            })?;
        let output_file = scratch.path().join("output.ndjson");

        let mut command = Command::new(&self.program);
        command
            .args(&self.prefix_args)
            .args(args)
            .current_dir(working_dir)
            .env_clear()
            .env("CLOUDFLARE_API_TOKEN", credential.api_token())
            .env("CLOUDFLARE_ACCOUNT_ID", credential.account_id())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("PATH", &self.path_env)
            .env("CI", "true")
            .env("NO_COLOR", "1")
            .env("FORCE_COLOR", "0")
            .env("WRANGLER_SEND_METRICS", "false")
            .env("WRANGLER_LOG_PATH", scratch.path().join("logs"))
            .env("WRANGLER_OUTPUT_FILE_PATH", &output_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if stdin_data.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|error| WranglerError::Spawn {
            context: format!("starting wrangler {command_text}"),
            error,
        })?;

        if let (Some(data), Some(mut stdin)) = (stdin_data, child.stdin.take()) {
            stdin
                .write_all(data)
                .await
                .map_err(|error| WranglerError::Io {
                    context: format!("writing stdin of wrangler {command_text}"),
                    error,
                })?;
            drop(stdin);
        }

        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| WranglerError::Timeout {
                duration: timeout,
                command: command_text.clone(),
            })?
            .map_err(|error| WranglerError::Io {
                context: format!("waiting for wrangler {command_text}"),
                error,
            })?;

        let records = std::fs::read_to_string(&output_file)
            .map(|contents| parse_ndjson(&contents))
            .unwrap_or_default();
        let stdout = credential.redact(&String::from_utf8_lossy(&output.stdout));
        let stderr = credential.redact(&String::from_utf8_lossy(&output.stderr));

        if !output.status.success() {
            let api_code = records
                .iter()
                .rev()
                .find(|record| record["type"] == "command-failed")
                .and_then(|record| match &record["code"] {
                    serde_json::Value::Number(number) => number.as_u64(),
                    serde_json::Value::String(text) => text.parse().ok(),
                    _ => None,
                })
                .or_else(|| find_api_code(&stderr))
                .or_else(|| find_api_code(&stdout));
            let message = records
                .iter()
                .rev()
                .find(|record| record["type"] == "command-failed")
                .and_then(|record| record["message"].as_str())
                .map(|message| credential.redact(message));
            return Err(WranglerError::CommandFailed {
                command: command_text,
                exit_code: output.status.code(),
                api_code,
                message,
                stderr: tail(&stderr),
                stdout: tail(&stdout),
            });
        }

        Ok(WranglerOutput {
            stdout,
            stderr,
            records,
        })
    }
}

/// A successful wrangler run.
#[derive(Clone, Debug, Default)]
pub struct WranglerOutput {
    pub stdout: String,
    pub stderr: String,
    /// The ND-JSON records wrangler appended to `WRANGLER_OUTPUT_FILE_PATH`.
    pub records: Vec<serde_json::Value>,
}

impl WranglerOutput {
    /// The last output-file record of the given `type`.
    pub fn record(&self, record_type: &str) -> Option<&serde_json::Value> {
        self.records
            .iter()
            .rev()
            .find(|record| record["type"] == record_type)
    }

    /// Parse stdout as one JSON document (the `--json` output of list
    /// commands, and `kv namespace list`, which always prints JSON).
    ///
    /// wrangler may print a banner or warnings before the document, so the
    /// parse starts at the first line that opens a JSON array or object.
    pub fn stdout_json(&self, command: &str) -> Result<serde_json::Value, WranglerError> {
        let start = self
            .stdout
            .lines()
            .scan(0usize, |offset, line| {
                let at = *offset;
                *offset += line.len() + 1;
                Some((at, line))
            })
            .find(|(_, line)| {
                let trimmed = line.trim_start();
                trimmed.starts_with('[') || trimmed.starts_with('{')
            })
            .map(|(at, _)| at);
        let parsed = start.and_then(|at| {
            serde_json::Deserializer::from_str(&self.stdout[at..])
                .into_iter::<serde_json::Value>()
                .next()
                .and_then(Result::ok)
        });
        parsed.ok_or_else(|| WranglerError::OutputParse {
            command: command.to_owned(),
            message: "stdout carries no JSON document".to_owned(),
            output: tail(&self.stdout),
        })
    }
}

/// Parse newline-delimited JSON, skipping lines that are not JSON.
pub fn parse_ndjson(contents: &str) -> Vec<serde_json::Value> {
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The Cloudflare API error code wrangler prints as `[code: 10007]`.
pub fn find_api_code(text: &str) -> Option<u64> {
    let start = text.rfind("[code: ")? + "[code: ".len();
    let digits = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().ok()
}

fn tail(text: &str) -> String {
    if text.len() <= OUTPUT_TAIL_BYTES {
        return text.to_owned();
    }
    let mut start = text.len() - OUTPUT_TAIL_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("...{}", &text[start..])
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
    Node {
        message: String,
    },
    Spawn {
        context: String,
        error: io::Error,
    },
    Timeout {
        duration: Duration,
        command: String,
    },
    OutputParse {
        command: String,
        message: String,
        output: String,
    },
    /// wrangler ran and exited non-zero.
    CommandFailed {
        command: String,
        exit_code: Option<i32>,
        /// The Cloudflare API error code, when wrangler reported one.
        api_code: Option<u64>,
        /// wrangler's own error message from its `command-failed` record.
        message: Option<String>,
        stderr: String,
        stdout: String,
    },
}

impl WranglerError {
    /// The Cloudflare API error code of a failed command.
    pub fn api_code(&self) -> Option<u64> {
        match self {
            Self::CommandFailed { api_code, .. } => *api_code,
            _ => None,
        }
    }
}

impl std::fmt::Display for WranglerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { context, error } => write!(f, "{context}: {error}"),
            Self::Download { url, message } => write!(f, "download {url}: {message}"),
            Self::Integrity { message } => write!(f, "integrity check failed: {message}"),
            Self::Node { message } => write!(f, "node runtime: {message}"),
            Self::Spawn { context, error } => write!(f, "{context}: {error}"),
            Self::Timeout { duration, command } => {
                write!(f, "wrangler {command} timed out after {duration:?}")
            }
            Self::OutputParse {
                command,
                message,
                output,
            } => write!(
                f,
                "could not read the output of wrangler {command}: {message}\n---stdout---\n{output}\n---"
            ),
            Self::CommandFailed {
                command,
                exit_code,
                api_code,
                message,
                stderr,
                ..
            } => {
                write!(f, "wrangler {command} exited with {exit_code:?}")?;
                if let Some(code) = api_code {
                    write!(f, " (Cloudflare API code {code})")?;
                }
                if let Some(message) = message {
                    write!(f, ": {message}")?;
                }
                if !stderr.trim().is_empty() {
                    write!(f, "\n---stderr---\n{}\n---", stderr.trim_end())?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for WranglerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ndjson_skips_non_json_lines() {
        let records = parse_ndjson("not json\n{\"type\":\"a\"}\n\n{\"type\":\"b\"}\n");
        assert_eq!(records.len(), 2);
        assert_eq!(records[1]["type"], "b");
    }

    #[test]
    fn api_code_is_read_from_wrangler_error_text() {
        assert_eq!(
            find_api_code("✘ [ERROR] A request to the Cloudflare API failed. [code: 10007]\n"),
            Some(10007)
        );
        assert_eq!(find_api_code("no code here"), None);
    }

    #[test]
    fn stdout_json_skips_a_banner() {
        let output = WranglerOutput {
            stdout: "\n ⛅️ wrangler 4.129.0\n───────\n[\n  {\"id\": \"a\", \"title\": \"t\"}\n]\n"
                .to_owned(),
            ..WranglerOutput::default()
        };
        let value = output.stdout_json("kv namespace list").unwrap();
        assert_eq!(value[0]["id"], "a");
    }

    #[test]
    fn tail_keeps_the_end() {
        let long = "x".repeat(OUTPUT_TAIL_BYTES * 2) + "END";
        assert!(tail(&long).ends_with("END"));
        assert!(tail(&long).len() < long.len());
    }
}
