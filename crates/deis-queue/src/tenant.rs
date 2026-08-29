//! The one tenant dimension this service understands: `app_id`.
//!
//! It arrives out of band in the `x-deis-app-id` gRPC metadata header, the
//! same header deis-proxy already stamps on the HTTP data path, so the
//! existing queue.proto messages need no tenant field.
//!
//! The rules are character-for-character the ones deis-store applies. The
//! queue keeps everything in one SQLite file, so an id never names a path
//! here and traversal is not the danger it is there - but an id that
//! deis-store refuses and this service accepts would mean an app could own a
//! queue it can never own storage for, and the two services disagreeing about
//! what an app id is would surface as a mystery much later. One definition,
//! applied twice.

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::APP_ID_HEADER;

use crate::error::QueueError;
use std::fmt;
use tonic::Request;

/// Longest accepted tenant id. Matches deis-store, where the limit comes from
/// the id also being a filename.
const MAX_APP_ID_LEN: usize = 64;

/// A validated tenant id.
///
/// The only way to build one is [`AppId::parse`], so any `AppId` below is
/// already known to be printable, bounded and free of separators.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AppId(String);

impl AppId {
    /// Validate `raw`. Accepts ASCII letters, digits, `_` and `-` only.
    pub fn parse(raw: &str) -> Result<Self, QueueError> {
        if raw.is_empty() {
            return Err(QueueError::Tenant("app id is empty".to_string()));
        }
        if raw.len() > MAX_APP_ID_LEN {
            return Err(QueueError::Tenant(format!(
                "app id is {} bytes, limit is {MAX_APP_ID_LEN}",
                raw.len()
            )));
        }
        if let Some(bad) = raw
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-'))
        {
            return Err(QueueError::Tenant(format!(
                "app id contains {bad:?}, only ASCII letters, digits, '_' and '-' are allowed"
            )));
        }
        Ok(Self(raw.to_string()))
    }

    /// The id as it appears in metadata and in the `app_id` column.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AppId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Pull the tenant out of a request's metadata.
///
/// A missing or malformed header is refused before any message is touched.
/// It is logged at warn rather than error: the call is lost, but nothing
/// already queued is affected, and the usual cause is a caller that forgot
/// the header rather than a fault in the queue.
pub fn app_id_of<T>(request: &Request<T>) -> Result<AppId, QueueError> {
    let raw = request
        .metadata()
        .get(APP_ID_HEADER)
        .ok_or_else(|| {
            tracing::warn!(
                header = APP_ID_HEADER,
                "request has no tenant header, refusing it; the caller must stamp the app id or it \
                 would be consuming another app's messages"
            );
            QueueError::Tenant(format!("missing {APP_ID_HEADER} header"))
        })?
        .to_str()
        .map_err(|_| {
            tracing::warn!(
                header = APP_ID_HEADER,
                "tenant header is not printable ASCII, refusing the request"
            );
            QueueError::Tenant(format!("{APP_ID_HEADER} is not valid ASCII"))
        })?;

    AppId::parse(raw).inspect_err(|e| {
        tracing::warn!(
            raw = %raw.escape_debug(),
            error = %e,
            "rejected tenant id; the request is refused and no message is enqueued or delivered"
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_app_names() {
        for id in ["my-app", "_system", "a", "App_9", &"x".repeat(64)] {
            assert!(AppId::parse(id).is_ok(), "{id} should be accepted");
        }
    }

    #[test]
    fn rejects_the_same_ids_deis_store_rejects() {
        for id in [
            "..",
            ".",
            "../etc",
            "a/b",
            "a\\b",
            "a.sqlite",
            "a b",
            "app\0",
            "",
            &"x".repeat(65),
        ] {
            assert!(
                AppId::parse(id).is_err(),
                "{} should be rejected",
                id.escape_debug()
            );
        }
    }

    #[test]
    fn reads_the_tenant_from_metadata() {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(APP_ID_HEADER, "my-app".parse().unwrap());
        assert_eq!(app_id_of(&req).unwrap().as_str(), "my-app");
    }

    #[test]
    fn a_call_with_no_tenant_is_refused() {
        let req = Request::new(());
        let err = app_id_of(&req).unwrap_err();
        assert!(matches!(err, QueueError::Tenant(_)));
    }

    #[test]
    fn a_malformed_tenant_in_metadata_is_refused() {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(APP_ID_HEADER, "../../etc".parse().unwrap());
        assert!(matches!(app_id_of(&req), Err(QueueError::Tenant(_))));
    }
}
