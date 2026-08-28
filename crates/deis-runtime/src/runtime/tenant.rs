//! The tenant every storage call is charged to.
//!
//! deis-store knows exactly one dimension of the world outside itself: the
//! `app_id`. It arrives out of band, in the [`crate::APP_ID_HEADER`] gRPC
//! metadata header, which is why none of the storage proto messages carry a
//! tenant field. A request that arrives without the header is refused with
//! PermissionDenied before any storage is touched, so every storage client in
//! this crate holds a [`Tenant`] and stamps it on every request it sends.

use anyhow::{anyhow, Result};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::Request;

/// One application's identity on the storage wire.
///
/// The metadata value is parsed once, at construction, so an id that cannot
/// be sent as a header fails when the client is built rather than on every
/// call afterwards.
#[derive(Clone, Debug)]
pub struct Tenant {
    app_id: String,
    value: MetadataValue<Ascii>,
}

impl Tenant {
    /// Bind to one application.
    ///
    /// deis-store additionally restricts ids to ASCII letters, digits, `_`
    /// and `-`, because the id names files on disk. This check is the weaker
    /// transport-level one; the store stays the authority and will refuse an
    /// id this accepts but it does not.
    pub fn new(app_id: impl Into<String>) -> Result<Self> {
        let app_id = app_id.into();
        if app_id.is_empty() {
            return Err(anyhow!(
                "storage client was given an empty app id; every storage call would be refused"
            ));
        }
        let value: MetadataValue<Ascii> = app_id.parse().map_err(|_| {
            anyhow!(
                "app id {app_id:?} cannot be sent as gRPC metadata; storage would be unusable \
                 for this application"
            )
        })?;
        Ok(Self { app_id, value })
    }

    /// The application id as the store will see it.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Wrap a message in a request carrying this tenant.
    pub fn stamp<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert(crate::APP_ID_HEADER, self.value.clone());
        request
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_the_header_the_store_reads() {
        let tenant = Tenant::new("demo-app_1").expect("plain ascii id");
        let request = tenant.stamp(());
        assert_eq!(
            request
                .metadata()
                .get(crate::APP_ID_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("demo-app_1")
        );
    }

    #[test]
    fn keeps_the_app_id_readable_for_logs() {
        let tenant = Tenant::new("demo").expect("plain ascii id");
        assert_eq!(tenant.app_id(), "demo");
    }

    #[test]
    fn refuses_an_empty_app_id() {
        let err = Tenant::new("").expect_err("an empty id names no application");
        assert!(err.to_string().contains("empty app id"), "{err}");
    }

    #[test]
    fn refuses_an_app_id_that_cannot_be_a_header() {
        let err = Tenant::new("demo\napp").expect_err("a newline cannot go in a header");
        assert!(err.to_string().contains("gRPC metadata"), "{err}");
    }
}
