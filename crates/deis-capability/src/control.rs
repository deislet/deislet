//! Reaching the control plane's machine channel from a verifier.
//!
//! deis-store, deis-queue and deis-telemetry read two things from the control
//! plane: the keys capabilities are signed with (the JWKS) and, beside them,
//! what has been revoked. Both decide which bearer tokens a service honours,
//! so both have to arrive from the control plane and from nobody else: a
//! substituted key set lets whoever substituted it mint any tenant's
//! capability, and a substituted revocation list can undo a revocation or
//! refuse every call on the platform.
//!
//! So they are read over the machine channel, under mutual TLS, with the
//! service proving its own machine identity and checking that the far end
//! proves the control plane's name in the same trust domain. There is no
//! plaintext fallback: an `http://` URL is accepted only when it names this
//! host's loopback interface, where nothing on the network can sit between
//! the two processes, and is refused at startup anywhere else.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use deis_identity::{
    IdentityError, IdentityFiles, MachineIdentity, Protocol, ServiceKind, SpiffeId,
};
use serde::de::DeserializeOwned;

/// How often the client's own identity files are re-read when
/// [`ControlClient::connect`] loaded them.
const IDENTITY_RELOAD: Duration = Duration::from_secs(60);

/// Where the control plane's machine channel is and what to prove to it.
///
/// `url` is the base of the machine channel, `https://host:port`; paths are
/// appended to it. `ca_bundle` is the trust root the control plane's
/// certificate must chain to, and `client_certificate`/`client_private_key`
/// are this service's own machine identity. The control plane's expected
/// name is derived from the trust domain of that identity - there is one
/// control plane per trust domain - so it needs no setting of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlClientConfig {
    pub url: String,
    pub ca_bundle: PathBuf,
    pub client_certificate: PathBuf,
    pub client_private_key: PathBuf,
}

impl ControlClientConfig {
    /// The three files, as `deis_identity` loads them.
    pub fn identity_files(&self) -> IdentityFiles {
        IdentityFiles::new(
            self.client_certificate.clone(),
            self.client_private_key.clone(),
            self.ca_bundle.clone(),
        )
    }
}

/// The `[machine_identity]` table of deis-store, deis-queue and
/// deis-telemetry: the certificate each serves its gRPC listener with, and
/// the control plane's machine channel it reads keys and revocations from.
///
/// One table for both, because they are one identity: the certificate the
/// service presents to its callers is the one it proves to the control
/// plane, and a service configured with half of it has either nothing to
/// serve with or nowhere to get its keys.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceIdentityConfig {
    /// This service's certificate chain, end-entity certificate first. It
    /// must name this kind of service -
    /// `spiffe://<trust domain>/deislet/store/<id>` for a store.
    pub certificate: PathBuf,
    /// The private key belonging to that certificate.
    pub private_key: PathBuf,
    /// The fleet's trust root: what the control plane's certificate must
    /// chain to, and what every caller of this service checks its
    /// certificate against.
    pub trust_root: PathBuf,
    /// The control plane's machine channel, `https://host:port`. The JWKS and
    /// the signed revocation list are read from it; `http://` is accepted
    /// only to a loopback address.
    pub control_endpoint: String,
    /// How often the three files are re-read, in seconds, so a renewed
    /// certificate is adopted without a restart.
    #[serde(default = "default_identity_reload_seconds")]
    pub identity_reload_seconds: u64,
}

fn default_identity_reload_seconds() -> u64 {
    60
}

impl ServiceIdentityConfig {
    /// The three files, as `deis_identity` loads them.
    pub fn identity_files(&self) -> IdentityFiles {
        IdentityFiles::new(
            self.certificate.clone(),
            self.private_key.clone(),
            self.trust_root.clone(),
        )
    }

    /// The control-plane side of this table.
    pub fn control_client_config(&self) -> ControlClientConfig {
        ControlClientConfig {
            url: self.control_endpoint.clone(),
            ca_bundle: self.trust_root.clone(),
            client_certificate: self.certificate.clone(),
            client_private_key: self.private_key.clone(),
        }
    }

    /// Load the identity, refuse it unless it names a `kind`, and keep it in
    /// step with its files. Must be called inside a tokio runtime.
    ///
    /// The kind is checked here and not left to the first caller: a store
    /// started with a node's certificate would be refused by every client,
    /// each with a handshake error, when it can be refused once at startup
    /// with a message naming the file.
    pub fn load(&self, kind: ServiceKind) -> Result<Arc<MachineIdentity>, ControlClientError> {
        let identity = Arc::new(MachineIdentity::load(
            self.identity_files(),
            &[Protocol::Http2],
        )?);
        let id = identity.id();
        if deis_identity::names::service_kind(&id) != Some(kind) {
            return Err(ControlClientError::WrongKind {
                certificate: self.certificate.clone(),
                presented: id.to_string(),
                expected: kind,
            });
        }
        identity.watch(Duration::from_secs(self.identity_reload_seconds.max(1)));
        Ok(identity)
    }

    /// The control plane client, proving `identity` (from [`Self::load`]).
    pub fn control_client(
        &self,
        identity: Arc<MachineIdentity>,
    ) -> Result<ControlClient, ControlClientError> {
        ControlClient::with_identity(&self.control_endpoint, identity)
    }
}

/// Why a [`ControlClient`] could not be built.
#[derive(Debug, thiserror::Error)]
pub enum ControlClientError {
    #[error(
        "{url} is plain HTTP to another host. Whatever sits on the path could replace the \
         control plane's signing keys or its revocation list and so decide which capabilities \
         this service honours; use the control plane's machine channel, https://host:port, \
         which this service reaches under mutual TLS"
    )]
    PlaintextOffHost { url: String },
    #[error("{url} is not an http(s) URL this service can reach the control plane at: {reason}")]
    Unusable { url: String, reason: String },
    #[error("the machine identity this service reaches the control plane with did not load: {0}")]
    Identity(#[from] IdentityError),
    #[error("cannot name the control plane of this service's trust domain: {0}")]
    ControlName(String),
    #[error(
        "the certificate in {certificate} names {presented}, which is not a {expected:?} of this \
         fleet; issue this service a certificate for spiffe://<trust domain>/deislet/<kind>/<id>"
    )]
    WrongKind {
        certificate: PathBuf,
        presented: String,
        expected: ServiceKind,
    },
}

#[derive(Clone)]
enum Transport {
    /// Mutual TLS: prove `identity`, accept only `control`.
    Mutual {
        identity: Arc<MachineIdentity>,
        control: SpiffeId,
    },
    /// Plain HTTP to this host's loopback interface.
    Loopback,
}

/// A client for the control plane's machine channel.
///
/// Cheap to clone. The HTTP client is built per request from the identity
/// in force at that moment, so a renewed certificate is presented from the
/// next request on; a request every few seconds at most makes that free.
#[derive(Clone)]
pub struct ControlClient {
    base: String,
    transport: Transport,
    timeout: Duration,
}

impl std::fmt::Debug for ControlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlClient")
            .field("url", &self.base)
            .field(
                "transport",
                &match &self.transport {
                    Transport::Mutual { control, .. } => format!("mutual TLS to {control}"),
                    Transport::Loopback => "plaintext loopback".to_string(),
                },
            )
            .finish()
    }
}

impl ControlClient {
    /// Load the identity in `config` and reach the control plane at its
    /// `url`. Must be called inside a tokio runtime: the identity files are
    /// re-read in the background, so a renewal needs no restart.
    pub fn connect(config: &ControlClientConfig) -> Result<Self, ControlClientError> {
        let identity = Arc::new(MachineIdentity::load(config.identity_files(), &[])?);
        identity.watch(IDENTITY_RELOAD);
        Self::with_identity(&config.url, identity)
    }

    /// Reach the control plane at `url`, proving `identity`: for a process
    /// that already holds its machine identity for something else, such as
    /// the listener it serves.
    pub fn with_identity(
        url: &str,
        identity: Arc<MachineIdentity>,
    ) -> Result<Self, ControlClientError> {
        let base = checked_base(url)?;
        let transport = if base.starts_with("https://") {
            let control = deis_identity::names::control(identity.id().trust_domain())
                .map_err(|e| ControlClientError::ControlName(e.to_string()))?;
            Transport::Mutual { identity, control }
        } else {
            Transport::Loopback
        };
        Ok(Self {
            base,
            transport,
            timeout: Duration::from_secs(10),
        })
    }

    /// Reach a control plane on this host's loopback interface over plain
    /// HTTP. Anything else is refused, exactly as [`Self::connect`] refuses
    /// it; `https://` needs an identity and so one of the other constructors.
    pub fn loopback(url: &str) -> Result<Self, ControlClientError> {
        let base = checked_base(url)?;
        if base.starts_with("https://") {
            return Err(ControlClientError::Unusable {
                url: url.to_string(),
                reason: "an https:// URL is reached under mutual TLS, which needs this \
                         service's machine identity"
                    .to_string(),
            });
        }
        Ok(Self {
            base,
            transport: Transport::Loopback,
            timeout: Duration::from_secs(10),
        })
    }

    /// The same client with a different per-request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The machine channel's base URL.
    pub fn url(&self) -> &str {
        &self.base
    }

    /// The full URL of `path` on the machine channel.
    pub fn url_of(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    fn http(&self) -> Result<reqwest::Client, String> {
        let builder = reqwest::Client::builder().timeout(self.timeout);
        let builder = match &self.transport {
            Transport::Mutual { identity, control } => {
                let tls = identity
                    .client_config(control, &[Protocol::Http11])
                    .map_err(|e| format!("cannot build the mutual TLS configuration: {e}"))?;
                builder.use_preconfigured_tls((*tls).clone())
            }
            Transport::Loopback => builder,
        };
        builder
            .build()
            .map_err(|e| format!("cannot build the HTTP client: {e}"))
    }

    /// GET `path` and parse the body as JSON.
    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let url = self.url_of(path);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("request to {url} failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("{url} answered {status}"));
        }
        response
            .json()
            .await
            .map_err(|e| format!("the body {url} answered with is not what was expected: {e}"))
    }
}

/// `url` without a trailing slash, if it is one this service may use.
fn checked_base(url: &str) -> Result<String, ControlClientError> {
    let parsed = reqwest::Url::parse(url).map_err(|e| ControlClientError::Unusable {
        url: url.to_string(),
        reason: e.to_string(),
    })?;
    match parsed.scheme() {
        "https" => {}
        "http" if is_loopback(&parsed) => {}
        "http" => {
            return Err(ControlClientError::PlaintextOffHost {
                url: url.to_string(),
            });
        }
        other => {
            return Err(ControlClientError::Unusable {
                url: url.to_string(),
                reason: format!("the scheme {other} is neither https nor http"),
            });
        }
    }
    if parsed.host_str().is_none() {
        return Err(ControlClientError::Unusable {
            url: url.to_string(),
            reason: "it names no host".to_string(),
        });
    }
    Ok(url.trim_end_matches('/').to_string())
}

/// Whether `url` names this host's loopback interface by address.
///
/// `localhost` is not taken on trust: it is whatever the resolver says, and a
/// hosts file or a search domain can point it off the machine.
fn is_loopback(url: &reqwest::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_to_another_host_is_refused() {
        for url in [
            "http://10.0.0.5:8101",
            "http://control.internal:8101",
            "http://localhost:8101",
            "http://0.0.0.0:8101",
        ] {
            assert!(
                matches!(
                    ControlClient::loopback(url),
                    Err(ControlClientError::PlaintextOffHost { .. })
                ),
                "{url} was accepted"
            );
        }
    }

    #[test]
    fn plain_http_to_loopback_is_accepted() {
        for url in ["http://127.0.0.1:8101", "http://[::1]:8101/"] {
            let client = ControlClient::loopback(url).expect(url);
            assert!(!client.url().ends_with('/'));
            assert_eq!(
                client.url_of("/.well-known/x"),
                format!("{}/.well-known/x", client.url())
            );
        }
    }

    #[test]
    fn other_schemes_are_refused() {
        for url in ["ftp://127.0.0.1/", "not a url", "unix:///run/control.sock"] {
            assert!(ControlClient::loopback(url).is_err(), "{url} was accepted");
        }
    }
}
