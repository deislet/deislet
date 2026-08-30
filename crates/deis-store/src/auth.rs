//! Proving that a caller may act as the tenant it names.
//!
//! # The hole this closes
//!
//! Every request names its tenant in `x-deis-tenant`, and until this existed
//! the service simply believed it. The only thing keeping one application
//! away from another application's data was that the port was bound to
//! loopback and the egress policy kept application code off it - two
//! unrelated settings, either of which an operator could widen without
//! realising it was the whole of the authorisation.
//!
//! What a caller now presents alongside the tenant is a capability token:
//! `base64(HMAC-SHA256(auth_secret, tenant))`, in
//! [`STORAGE_TOKEN_HEADER`](deis_wire::conventions::STORAGE_TOKEN_HEADER).
//! The secret is configured here and on deis-control, which mints one token
//! per tenant and hands it to a node with that tenant's deployment. The check
//! below derives the expected token from *the tenant the request itself
//! claims* and compares, so there is no lookup from token to tenant and a
//! token that leaked out of one application authorises nothing for another.
//!
//! It is a capability, not a session: it does not expire and it carries no
//! identity beyond "the holder may be this tenant". Rotating `auth_secret`
//! invalidates every token at once, which is the lever an operator has.
//!
//! # What an unconfigured service does
//!
//! Two cases, and they are deliberately not the same:
//!
//! * **Loopback with no secret** keeps working, untouched. That is the
//!   default single-box install, where deis-runtime and deis-store share a
//!   host and the port is unreachable from anywhere else. It is not silent:
//!   [`StorageAuth::for_listener`] logs one warning at startup naming exactly
//!   what is and is not protecting the data.
//! * **Any other address with no secret** refuses to start. Unconfigured must
//!   not quietly mean unguarded, and a non-loopback bind is the moment the
//!   containment that stood in for authentication stops existing. This is the
//!   same shape as the node channel on deis-control, where an empty key list
//!   shuts the channel rather than opening it.
//!
//! # Where the check runs
//!
//! Once, as a tonic interceptor applied in [`crate::server`] to every service
//! the process registers. It runs before any handler, so no storage path can
//! forget it, and it sits outside the handlers precisely so that adding a
//! fourth service cannot add a fourth unguarded door - see `server::guarded`.
//! The health service is deliberately left open: it reports whether this
//! process can answer, names no tenant and reads no data, and a liveness
//! probe that needs a tenant credential is a probe the control plane cannot
//! make.

use crate::config::Config;
use crate::error::StoreError;
use deis_wire::conventions::{
    STORAGE_TOKEN_HEADER, TENANT_HEADER, storage_token, storage_token_matches,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tonic::{Request, Status};

/// What the process checks on every storage call.
///
/// Cheap to clone: the secret is shared, so one instance guards all three
/// services.
#[derive(Clone)]
pub struct StorageAuth {
    /// `None` means no secret is configured, which is only reachable on a
    /// loopback bind - [`StorageAuth::for_listener`] refuses to build one
    /// otherwise.
    secret: Option<Arc<String>>,
}

impl StorageAuth {
    /// Decide what guards this service, or refuse to serve at all.
    ///
    /// Called once, from [`crate::server::serve`], with the address actually
    /// bound rather than the one in the file: `listen` may name a hostname,
    /// and it is what the socket resolved to that decides who can reach it.
    ///
    /// This is also where the security posture of the process is written to
    /// the log, once, so that the first lines of a startup log say which of
    /// the two regimes is in force.
    pub fn for_listener(config: &Config, addr: SocketAddr) -> anyhow::Result<Self> {
        let secret = config
            .auth_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        match (secret, addr.ip().is_loopback()) {
            (Some(secret), loopback) => {
                tracing::info!(
                    %addr,
                    header = STORAGE_TOKEN_HEADER,
                    loopback,
                    "storage calls must present a capability token for the tenant they name; a \
                     call with no token, or with a token minted for another tenant or under \
                     another secret, is refused before any data is touched"
                );
                Ok(Self {
                    secret: Some(Arc::new(secret.to_string())),
                })
            }
            (None, true) => {
                // The default single-box install. Kept working on purpose,
                // but never silently: this line is the whole of the trust
                // model, and an operator has to be able to find it.
                tracing::warn!(
                    %addr,
                    "no auth_secret is configured, so callers are not authenticated at all: the \
                     tenant header is taken at face value and any process on this host can read, \
                     overwrite and delete any tenant's KV, SQL and blob data by naming its tenant \
                     id. What protects the data is only that this address is unreachable from \
                     another host. Set auth_secret here and on deis-control before this service \
                     is bound anywhere else"
                );
                Ok(Self { secret: None })
            }
            (None, false) => {
                tracing::error!(
                    %addr,
                    "refusing to start: this address is reachable from other hosts and no \
                     auth_secret is configured, so anything that can route to it could read and \
                     delete every tenant's data by naming a tenant id. Set auth_secret here and \
                     on deis-control, or bind a loopback address"
                );
                Err(anyhow::anyhow!(
                    "{addr} is not a loopback address and no auth_secret is configured; set \
                     auth_secret or bind a loopback address"
                ))
            }
        }
    }

    /// A service that requires tokens derived from `secret`.
    pub fn with_secret(secret: impl Into<String>) -> Self {
        Self {
            secret: Some(Arc::new(secret.into())),
        }
    }

    /// Whether a token is required. Reported at startup so the log says which
    /// regime is in force rather than leaving it to be inferred.
    pub fn is_enforcing(&self) -> bool {
        self.secret.is_some()
    }

    /// Refuse the request unless it proves it may be the tenant it names.
    ///
    /// The tenant string is used exactly as it arrived, without validation:
    /// shape is [`crate::tenant`]'s job, and doing it here would only decide
    /// which refusal a bad caller gets. What matters is that the expected
    /// token is derived from the claimed tenant, so a token can never
    /// authorise a tenant other than its own.
    pub fn check<T>(&self, request: &Request<T>) -> Result<(), StoreError> {
        let Some(secret) = self.secret.as_deref() else {
            return Ok(());
        };

        let tenant = request
            .metadata()
            .get(TENANT_HEADER)
            .and_then(|value| value.to_str().ok());
        let Some(tenant) = tenant else {
            tracing::warn!(
                header = TENANT_HEADER,
                "refused a storage call that names no readable tenant; with a secret configured \
                 there is no tenant to check a token against, so nothing is served"
            );
            return Err(StoreError::Unauthenticated(format!(
                "missing or unreadable {TENANT_HEADER} header"
            )));
        };

        // The token itself is never logged: it is the credential, and a log
        // that carries it is a log that grants access to whoever reads it.
        let presented = request
            .metadata()
            .get(STORAGE_TOKEN_HEADER)
            .and_then(|value| value.to_str().ok());
        let Some(presented) = presented else {
            tracing::warn!(
                tenant = %tenant.escape_debug(),
                header = STORAGE_TOKEN_HEADER,
                "refused a storage call that carried no capability token; the caller was handed \
                 one with its deployment, so this is a caller that is not deis-runtime or one \
                 whose token was never delivered"
            );
            return Err(StoreError::Unauthenticated(format!(
                "missing {STORAGE_TOKEN_HEADER} header"
            )));
        };

        if !storage_token_matches(&storage_token(secret, tenant), presented) {
            tracing::warn!(
                tenant = %tenant.escape_debug(),
                "refused a storage call whose capability token does not match the tenant it \
                 claims; either the token was minted for another tenant, or this service and \
                 deis-control no longer share the same auth_secret - in which case every call \
                 from every node is being refused"
            );
            return Err(StoreError::Unauthenticated(format!(
                "the {STORAGE_TOKEN_HEADER} presented is not valid for this tenant"
            )));
        }

        Ok(())
    }
}

/// Redacted on purpose. A derived `Debug` would put the shared secret into
/// every panic message and every log line that formats a service, which is
/// exactly the place a credential must never appear.
impl std::fmt::Debug for StorageAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageAuth")
            .field("enforcing", &self.is_enforcing())
            .finish()
    }
}

/// Applied by [`crate::server`] to every service, so the check runs before
/// any handler and no storage path has to remember it.
impl tonic::service::Interceptor for StorageAuth {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        self.check(&request)?;
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deis_wire::conventions::tenant_id;
    use tonic::service::Interceptor;

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    fn call(tenant: Option<&str>, token: Option<&str>) -> Request<()> {
        let mut request = Request::new(());
        if let Some(tenant) = tenant {
            request
                .metadata_mut()
                .insert(TENANT_HEADER, tenant.parse().unwrap());
        }
        if let Some(token) = token {
            request
                .metadata_mut()
                .insert(STORAGE_TOKEN_HEADER, token.parse().unwrap());
        }
        request
    }

    #[test]
    fn a_non_loopback_bind_with_no_secret_refuses_to_start() {
        // The whole point of failing closed: an operator who widens `listen`
        // and nothing else must not end up publishing every tenant's data.
        for text in ["0.0.0.0:8090", "192.168.1.10:8090", "[::]:8090"] {
            let error = match StorageAuth::for_listener(&Config::default(), addr(text)) {
                Ok(_) => panic!("{text} must not be served unauthenticated"),
                Err(e) => e,
            };
            assert!(
                format!("{error:#}").contains("auth_secret"),
                "the refusal must name the missing setting: {error:#}"
            );
        }
    }

    #[test]
    fn a_non_loopback_bind_with_a_secret_serves() {
        let config = Config {
            auth_secret: Some("shared".to_string()),
            ..Config::default()
        };
        let auth = StorageAuth::for_listener(&config, addr("0.0.0.0:8090")).unwrap();
        assert!(auth.is_enforcing());
    }

    #[test]
    fn loopback_with_no_secret_keeps_working() {
        // The default single-box install. It stays unauthenticated on
        // purpose; the startup warning is what makes that visible.
        for text in ["127.0.0.1:8090", "[::1]:8090"] {
            let auth = StorageAuth::for_listener(&Config::default(), addr(text)).unwrap();
            assert!(!auth.is_enforcing());
            assert!(auth.check(&call(Some("app.production"), None)).is_ok());
        }
    }

    #[test]
    fn a_blank_secret_is_not_a_secret() {
        // A key left as an empty string in a config file is an operator who
        // meant to fill it in, not one who chose to run without it.
        for blank in ["", "   "] {
            let config = Config {
                auth_secret: Some(blank.to_string()),
                ..Config::default()
            };
            assert!(StorageAuth::for_listener(&config, addr("0.0.0.0:8090")).is_err());
            let auth = StorageAuth::for_listener(&config, addr("127.0.0.1:8090")).unwrap();
            assert!(!auth.is_enforcing());
        }
    }

    #[test]
    fn a_matching_token_is_accepted() {
        let auth = StorageAuth::with_secret("shared");
        let tenant = tenant_id("shop", "production");
        let token = storage_token("shared", &tenant);
        assert!(auth.check(&call(Some(&tenant), Some(&token))).is_ok());
    }

    #[test]
    fn a_token_from_another_application_is_useless_here() {
        // The reason the token is derived per tenant: a token that leaked out
        // of one application must not open another application's data, nor
        // another environment of its own.
        let auth = StorageAuth::with_secret("shared");
        let mine = storage_token("shared", &tenant_id("shop", "production"));
        for other in [
            tenant_id("shop", "preview-7"),
            tenant_id("other-app", "production"),
        ] {
            let error = auth.check(&call(Some(&other), Some(&mine))).unwrap_err();
            assert!(
                matches!(error, StoreError::Unauthenticated(_)),
                "a token for shop.production must not serve {other}: {error}"
            );
        }
    }

    #[test]
    fn a_call_with_no_token_is_refused_once_a_secret_exists() {
        let auth = StorageAuth::with_secret("shared");
        let error = auth
            .check(&call(Some("shop.production"), None))
            .unwrap_err();
        assert!(matches!(error, StoreError::Unauthenticated(_)));
    }

    #[test]
    fn a_call_with_no_tenant_is_refused_before_anything_else() {
        let auth = StorageAuth::with_secret("shared");
        let token = storage_token("shared", "shop.production");
        assert!(matches!(
            auth.check(&call(None, Some(&token))),
            Err(StoreError::Unauthenticated(_))
        ));
    }

    #[test]
    fn a_token_minted_under_another_secret_is_refused() {
        // What a half-finished rotation looks like: the store has the new
        // secret, deis-control is still handing out tokens from the old one.
        let auth = StorageAuth::with_secret("new-secret");
        let stale = storage_token("old-secret", "shop.production");
        assert!(matches!(
            auth.check(&call(Some("shop.production"), Some(&stale))),
            Err(StoreError::Unauthenticated(_))
        ));
    }

    #[test]
    fn the_interceptor_refuses_with_an_unauthenticated_status() {
        // This is the form the server actually applies, so the status a
        // caller sees is checked here rather than inferred from `check`.
        let mut auth = StorageAuth::with_secret("shared");
        let status = match Interceptor::call(&mut auth, call(Some("shop.production"), None)) {
            Ok(_) => panic!("an unsigned call must be refused"),
            Err(status) => status,
        };
        assert_eq!(status.code(), tonic::Code::Unauthenticated);

        let token = storage_token("shared", "shop.production");
        assert!(
            Interceptor::call(&mut auth, call(Some("shop.production"), Some(&token))).is_ok(),
            "a correctly signed call must pass through"
        );
    }
}
