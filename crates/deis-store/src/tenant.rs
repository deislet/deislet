//! The one tenant dimension this service understands: `app_id`.
//!
//! It arrives out of band in the `x-deis-app-id` gRPC metadata header, the
//! same header deis-proxy already stamps on the HTTP data path, so the
//! existing storage.proto messages need no tenant field.
//!
//! The id also names files on disk (`db/<app_id>.sqlite`,
//! `blob/<app_id>/...`), so it is validated hard rather than sanitised: an id
//! that could escape the data directory is refused outright.

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::APP_ID_HEADER;

use crate::error::StoreError;
use std::fmt;
use tonic::Request;


/// Longest accepted tenant id. Well under any filesystem name limit even
/// after the `.sqlite` suffix.
const MAX_APP_ID_LEN: usize = 64;

/// A validated tenant id, safe to use as a single path component.
///
/// The only way to build one is [`AppId::parse`], so any `AppId` in the code
/// below is already known not to contain a separator, a dot, or anything else
/// that would resolve outside the data directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AppId(String);

impl AppId {
    /// Validate `raw`. Accepts ASCII letters, digits, `_` and `-` only.
    ///
    /// Dots are rejected on purpose: that single rule removes `.`, `..` and
    /// hidden-file names in one go, and it removes any argument about how
    /// many dots it takes to climb out of `<data_dir>/db`.
    pub fn parse(raw: &str) -> Result<Self, StoreError> {
        if raw.is_empty() {
            return Err(StoreError::Tenant("app id is empty".to_string()));
        }
        if raw.len() > MAX_APP_ID_LEN {
            return Err(StoreError::Tenant(format!(
                "app id is {} bytes, limit is {MAX_APP_ID_LEN}",
                raw.len()
            )));
        }
        if let Some(bad) = raw
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-'))
        {
            return Err(StoreError::Tenant(format!(
                "app id contains {bad:?}, only ASCII letters, digits, '_' and '-' are allowed"
            )));
        }
        Ok(Self(raw.to_string()))
    }

    /// The id as it appears in metadata and on disk.
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
/// A missing or malformed header is refused before any storage is touched.
/// It is logged at warn rather than error: the request is lost, but nothing
/// stored is affected, and the usual cause is a caller that forgot the header
/// rather than a fault in the store.
pub fn app_id_of<T>(request: &Request<T>) -> Result<AppId, StoreError> {
    let raw = request
        .metadata()
        .get(APP_ID_HEADER)
        .ok_or_else(|| {
            tracing::warn!(
                header = APP_ID_HEADER,
                "request has no tenant header, refusing it; the caller must stamp the app id or it \
                 would be reading another app's data"
            );
            StoreError::Tenant(format!("missing {APP_ID_HEADER} header"))
        })?
        .to_str()
        .map_err(|_| {
            tracing::warn!(
                header = APP_ID_HEADER,
                "tenant header is not printable ASCII, refusing the request"
            );
            StoreError::Tenant(format!("{APP_ID_HEADER} is not valid ASCII"))
        })?;

    AppId::parse(raw).inspect_err(|e| {
        tracing::warn!(
            raw = %raw.escape_debug(),
            error = %e,
            "rejected tenant id; the request is refused and nothing on disk is touched"
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
    fn rejects_ids_that_could_escape_the_data_dir() {
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
    fn missing_metadata_is_a_tenant_error() {
        let req = Request::new(());
        let err = app_id_of(&req).unwrap_err();
        assert!(matches!(err, StoreError::Tenant(_)));
    }

    #[test]
    fn traversal_attempt_in_metadata_is_refused() {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(APP_ID_HEADER, "../../etc".parse().unwrap());
        assert!(matches!(app_id_of(&req), Err(StoreError::Tenant(_))));
    }
}
