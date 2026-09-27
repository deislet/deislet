//! How this node reaches deis-store, deis-queue and deis-telemetry.
//!
//! Every call to those services carries a bearer capability, so the
//! connection it travels on is TLS, and the far end must prove - with a
//! certificate chaining to this fleet's trust root - that it is the kind of
//! service the call is for. A queue's certificate does not answer for a
//! store, and a machine that merely holds the store's address gets a refused
//! handshake, not the capability.
//!
//! The trust is this node's own machine identity, installed once at startup
//! ([`install`]); it follows the identity through a renewal and a rotated
//! trust root without anything here being told. The clients that dial these
//! services are built deep inside isolate setup, far from where the identity
//! is loaded, and there is exactly one identity per process, which is why it
//! is held here rather than threaded through every constructor on the way.
//!
//! `http://` is refused except to a loopback address given as an IP literal.
//! No Deislet data service serves plaintext, so a node configured that way
//! simply fails to connect; the exception exists for in-process test doubles.

use std::net::IpAddr;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use deis_identity::{Protocol, ServiceKind, ServiceTrust};
use tonic::transport::{Channel, Endpoint};

static TRUST: OnceLock<ServiceTrust> = OnceLock::new();

/// Install what the data services' certificates are checked against. The
/// first installation wins: there is one identity per process.
pub fn install(trust: ServiceTrust) {
    if TRUST.set(trust).is_err() {
        tracing::debug!("the data-service trust was already installed; keeping the first");
    }
}

/// What to dial `endpoint` with, or `None` for a loopback plaintext test
/// double.
fn connector(endpoint: &str, kind: ServiceKind) -> Result<Option<deis_identity::ServiceDialler>> {
    let uri: tonic::codegen::http::Uri = endpoint
        .parse()
        .with_context(|| format!("{endpoint:?} is not a usable gRPC address"))?;
    match uri.scheme_str() {
        Some("https") => {
            let trust = TRUST.get().with_context(|| {
                format!(
                    "{endpoint} is reached over TLS and this node has installed no trust to check \
                     its certificate against, so no capability is sent to it"
                )
            })?;
            Ok(Some(trust.dialler(kind, &[Protocol::Http2])))
        }
        Some("http") => {
            let loopback = uri
                .host()
                .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
                .and_then(|host| host.parse::<IpAddr>().ok())
                .is_some_and(|ip| ip.is_loopback());
            if !loopback {
                bail!(
                    "{endpoint} is plaintext to another host; the data services serve TLS only and \
                     a capability is never sent in the clear. Configure the https:// address"
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
) -> Result<Channel> {
    let channel = match connector(endpoint, kind)? {
        Some(dialler) => builder.connect_with_connector(dialler).await,
        None => builder.connect().await,
    };
    channel.with_context(|| format!("connecting to {endpoint}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_is_refused_off_loopback() {
        for endpoint in [
            "http://10.0.0.5:8090",
            "http://store.internal:8090",
            "http://localhost:8090",
        ] {
            assert!(
                connector(endpoint, ServiceKind::Store).is_err(),
                "{endpoint} was accepted"
            );
        }
        assert!(matches!(
            connector("http://127.0.0.1:8090", ServiceKind::Store),
            Ok(None)
        ));
        assert!(matches!(
            connector("http://[::1]:8090", ServiceKind::Store),
            Ok(None)
        ));
    }
}
