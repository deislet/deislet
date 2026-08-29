//! Downloading a bundle and proving it is the bundle that was promised.
//!
//! The control plane sends a pointer - a URL and a digest - and the node
//! fetches the bytes itself (docs/implementation-plan.zh.md section 4). Until
//! those bytes hash to the digest they are untrusted input, so nothing here
//! opens the archive, and the file is written under a temporary name that no
//! other part of the node reads.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, warn};

use super::credential::NodeCredential;

/// Why a bundle did not arrive, or arrived and could not be trusted.
#[derive(Debug)]
pub enum FetchError {
    /// The control plane could not be reached, or the transfer broke.
    Transport { url: String, reason: String },
    /// The control plane answered, but not with a bundle.
    Status { url: String, code: u16 },
    /// The transfer went over the node's ceiling and was cut off.
    TooLarge { url: String, limit: u64 },
    /// The download did not finish inside the allowed time.
    Timeout { url: String, seconds: u64 },
    /// Writing the download to disk failed.
    Io { path: PathBuf, reason: String },
    /// The bytes are not the bytes that were promised.
    DigestMismatch {
        url: String,
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport { url, reason } => {
                write!(f, "could not download the bundle from {url}: {reason}")
            }
            Self::Status { url, code } => write!(
                f,
                "the bundle URL {url} answered HTTP {code}; whatever came back is not an \
                 application bundle and was discarded"
            ),
            Self::TooLarge { url, limit } => write!(
                f,
                "the bundle at {url} went past the {limit} byte ceiling and the download was \
                 abandoned; raise bundles.max_bytes if the app really is this big"
            ),
            Self::Timeout { url, seconds } => write!(
                f,
                "the bundle at {url} did not finish downloading within {seconds}s and was \
                 abandoned"
            ),
            Self::Io { path, reason } => write!(
                f,
                "could not write the downloaded bundle to {}: {reason}",
                path.display()
            ),
            Self::DigestMismatch {
                url,
                expected,
                actual,
            } => write!(
                f,
                "the bundle downloaded from {url} hashes to {actual} but the control plane \
                 promised {expected}; the bytes were discarded without being opened"
            ),
        }
    }
}

impl std::error::Error for FetchError {}

/// A bundle that has been downloaded and matched against its digest.
#[derive(Debug)]
pub struct FetchedBundle {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

/// Fetches bundles over HTTP, streaming to disk and hashing as it goes.
pub struct BundleFetcher {
    client: reqwest::Client,
    max_bytes: u64,
    timeout: Duration,
    credential: Option<NodeCredential>,
}

impl BundleFetcher {
    /// `credential` is the node key. It is presented only when the bundle URL
    /// is the control plane this node was configured with; see
    /// [`NodeCredential`].
    pub fn new(
        timeout: Duration,
        max_bytes: u64,
        credential: Option<NodeCredential>,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            // Applies to the idle gap between chunks: a control plane that
            // stops sending mid-bundle must not hold this download open for
            // the whole outer deadline.
            .read_timeout(timeout)
            .build()?;
        Ok(Self {
            client,
            max_bytes,
            timeout,
            credential,
        })
    }

    /// Download `url` into `temp_path`, verifying it against `expected_sha`.
    ///
    /// On any failure - including a digest that does not match - the partial
    /// file is removed and the caller is left with whatever it was already
    /// running. Nothing reads `temp_path` until this returns `Ok`.
    pub async fn fetch(
        &self,
        url: &str,
        expected_sha: &str,
        temp_path: &Path,
    ) -> Result<FetchedBundle, FetchError> {
        let outcome = tokio::time::timeout(self.timeout, self.stream_to_file(url, temp_path)).await;

        let result = match outcome {
            Ok(result) => result,
            Err(_) => Err(FetchError::Timeout {
                url: url.to_string(),
                seconds: self.timeout.as_secs(),
            }),
        };

        let (bytes, digest) = match result {
            Ok(pair) => pair,
            Err(e) => {
                Self::discard(temp_path).await;
                return Err(e);
            }
        };

        if !digest.eq_ignore_ascii_case(expected_sha) {
            Self::discard(temp_path).await;
            return Err(FetchError::DigestMismatch {
                url: url.to_string(),
                expected: expected_sha.to_string(),
                actual: digest,
            });
        }

        debug!(url, bytes, "Bundle downloaded and its digest verified");
        Ok(FetchedBundle {
            path: temp_path.to_path_buf(),
            bytes,
            sha256: digest,
        })
    }

    async fn stream_to_file(
        &self,
        url: &str,
        temp_path: &Path,
    ) -> Result<(u64, String), FetchError> {
        let mut request = self.client.get(url);
        if let Some(credential) = &self.credential {
            if let Some(value) = credential.header_value_for(url) {
                request = request.header(reqwest::header::AUTHORIZATION, value);
            }
        }

        let response = request.send().await.map_err(|e| FetchError::Transport {
            url: url.to_string(),
            reason: e.to_string(),
        })?;

        if !response.status().is_success() {
            let code = response.status().as_u16();
            if code == 401 || code == 403 {
                error!(
                    url,
                    code,
                    "The bundle host refused this node's credential, so this version cannot be \
                     installed and the environment keeps running its previous version. Set \
                     control.secret to one of the control plane's node_preshared_keys"
                );
            }
            return Err(FetchError::Status {
                url: url.to_string(),
                code,
            });
        }

        // A declared length over the ceiling is refused before a byte is
        // written; the running total below still guards a missing or lying
        // Content-Length.
        if let Some(declared) = response.content_length() {
            if declared > self.max_bytes {
                return Err(FetchError::TooLarge {
                    url: url.to_string(),
                    limit: self.max_bytes,
                });
            }
        }

        if let Some(parent) = temp_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| FetchError::Io {
                    path: parent.to_path_buf(),
                    reason: e.to_string(),
                })?;
        }

        let mut file = tokio::fs::File::create(temp_path)
            .await
            .map_err(|e| FetchError::Io {
                path: temp_path.to_path_buf(),
                reason: e.to_string(),
            })?;

        let mut hasher = Sha256::new();
        let mut written: u64 = 0;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| FetchError::Transport {
                url: url.to_string(),
                reason: e.to_string(),
            })?;

            written = written.saturating_add(chunk.len() as u64);
            if written > self.max_bytes {
                return Err(FetchError::TooLarge {
                    url: url.to_string(),
                    limit: self.max_bytes,
                });
            }

            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(|e| FetchError::Io {
                path: temp_path.to_path_buf(),
                reason: e.to_string(),
            })?;
        }

        file.flush().await.map_err(|e| FetchError::Io {
            path: temp_path.to_path_buf(),
            reason: e.to_string(),
        })?;

        Ok((written, hex::encode(hasher.finalize())))
    }

    /// Remove a partial or rejected download.
    ///
    /// A failure to remove it is worth a line but not worth failing over: the
    /// name is never read, and the store sweeps the temp directory on start.
    async fn discard(path: &Path) {
        if let Err(e) = tokio::fs::remove_file(path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "Could not remove a rejected bundle download; it occupies disk until the \
                     next sweep of the bundle temp directory"
                );
            }
        }
    }
}

/// Hex sha256 of a slice, spelled the same way the control plane spells it.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
#[path = "fetch_test.rs"]
mod fetch_test;
