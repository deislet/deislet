//! How this CLI reaches deis-store, deis-queue and deis-telemetry directly.
//!
//! Each call to those services carries a bearer capability, so the
//! connection is TLS and the far end must prove, with a certificate chaining
//! to the fleet's trust root, that it is the kind of service the command
//! meant to reach. A person's machine holds no machine identity of its own;
//! what it holds is the fleet's trust root, named by `DEIS_SERVICES_CA` or
//! `[services] ca` in the project file (see [`crate::settings`]).
//!
//! The path is recorded once per command by [`configure`] and read at dial
//! time, so a command that never touches a data service is never refused for
//! a trust root it did not need.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use deis_identity::{Protocol, ServiceKind, ServiceTrust};
use tonic::transport::{Channel, Endpoint};

use crate::settings::ENV_SERVICES_CA;

static CONFIGURED: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Record where the fleet's trust root is, for the commands that dial a data
/// service.
pub fn configure(path: Option<PathBuf>) {
    *CONFIGURED.lock().unwrap_or_else(|p| p.into_inner()) = path;
}

fn trust() -> Result<ServiceTrust> {
    let path = CONFIGURED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .with_context(|| {
            format!(
                "the platform's data services are reached over TLS, and nothing says which \
                 authority their certificates chain to. Set {ENV_SERVICES_CA} to the fleet's \
                 trust root (the ca.crt the installation issued its machines from), or add it to \
                 the project file:\n\n  [services]\n  ca = \"/path/to/ca.crt\""
            )
        })?;
    ServiceTrust::from_trust_root(&path)
        .with_context(|| format!("cannot load the fleet's trust root from {}", path.display()))
}

/// What to dial `endpoint` with, or `None` for a loopback plaintext test
/// double - no Deislet data service serves plaintext.
fn connector(endpoint: &str, kind: ServiceKind) -> Result<Option<deis_identity::ServiceDialler>> {
    let uri: tonic::codegen::http::Uri = endpoint
        .parse()
        .with_context(|| format!("{endpoint:?} is not a usable address"))?;
    match uri.scheme_str() {
        Some("https") => Ok(Some(trust()?.dialler(kind, &[Protocol::Http2]))),
        Some("http") => {
            let loopback = uri
                .host()
                .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
                .and_then(|host| host.parse::<IpAddr>().ok())
                .is_some_and(|ip| ip.is_loopback());
            if !loopback {
                bail!(
                    "{endpoint} is plaintext to another host. The platform's data services serve \
                     TLS only, and a capability is never sent in the clear: use the https:// \
                     address"
                );
            }
            Ok(None)
        }
        _ => bail!("{endpoint:?} is neither an https:// nor an http:// address"),
    }
}

/// A channel to a `kind` service at `endpoint` that dials on first use.
pub(crate) fn lazy(builder: Endpoint, endpoint: &str, kind: ServiceKind) -> Result<Channel> {
    Ok(match connector(endpoint, kind)? {
        Some(dialler) => builder.connect_with_connector_lazy(dialler),
        None => builder.connect_lazy(),
    })
}

/// A channel to a `kind` service at `endpoint`, dialled now.
pub(crate) async fn connect(
    builder: Endpoint,
    endpoint: &str,
    kind: ServiceKind,
) -> Result<Channel, anyhow::Error> {
    match connector(endpoint, kind)? {
        Some(dialler) => Ok(builder.connect_with_connector(dialler).await?),
        None => Ok(builder.connect().await?),
    }
}

/// The trust every test server in this binary is issued a certificate by,
/// configured as this CLI's trust root.
#[cfg(test)]
pub(crate) fn test_fleet() -> &'static deis_identity_test_pki::TestFleet {
    static FLEET: std::sync::OnceLock<deis_identity_test_pki::TestFleet> =
        std::sync::OnceLock::new();
    let fleet = FLEET.get_or_init(deis_identity_test_pki::TestFleet::new);
    configure(Some(fleet.trust_root()));
    fleet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_is_refused_off_loopback() {
        for endpoint in ["http://10.0.0.5:8090", "http://store.internal:8090"] {
            assert!(
                connector(endpoint, ServiceKind::Store).is_err(),
                "{endpoint}"
            );
        }
        assert!(matches!(
            connector("http://127.0.0.1:8090", ServiceKind::Store),
            Ok(None)
        ));
    }
}
