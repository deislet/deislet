//! One error type for every storage path, plus the mapping to gRPC status.
//!
//! The split that matters here: a *transport* failure (bad tenant header,
//! unreadable disk, poisoned database) becomes a gRPC status, while a
//! *statement* failure (the tenant sent SQL that does not parse) is reported
//! inside the response body, because storage.proto gives every response an
//! `error` field for exactly that. Mixing the two would make a typo in an
//! tenant's SQL look like the store is down.

use std::fmt;
use tonic::{Code, Status};

/// Anything that can go wrong below the gRPC layer.
#[derive(Debug)]
pub enum StoreError {
    /// The caller did not identify a usable tenant.
    Tenant(String),
    /// The caller did not prove it may act as the tenant it named. Distinct
    /// from [`StoreError::Tenant`], which is a tenant id the service cannot
    /// use at all: this one is well formed and simply not the caller's to
    /// name. See [`crate::auth`].
    Unauthenticated(String),
    /// The caller sent a request the store cannot act on at all.
    InvalidArgument(String),
    /// The addressed object does not exist.
    NotFound(String),
    /// Metadata and content disagree: something that should be there is gone.
    Corrupt(String),
    /// The request exceeds a configured ceiling.
    TooLarge(String),
    /// The caller asked for a byte range that names no byte of the object it
    /// addressed. Deliberately not [`StoreError::InvalidArgument`]: the
    /// request is perfectly well formed and would have been satisfiable
    /// against a longer object, so what the caller has to change is the range
    /// it asks for, not the shape of its request. This is the case HTTP
    /// answers 416 for, and telling it apart from [`StoreError::NotFound`] is
    /// what lets an application answer 416 rather than 404.
    RangeNotSatisfiable(String),
    /// A per-server or per-tenant ceiling on how many of something may be
    /// open at once is already reached. Nothing the caller sent is wrong and
    /// the same request may well succeed later, which is what separates this
    /// from [`StoreError::InvalidArgument`].
    Exhausted(String),
    /// A deadline the server set has passed: a handle the caller still holds
    /// was closed because it outlived its own, or a statement was stopped
    /// part-way because the request had spent the time it was given - or
    /// because whoever asked for it stopped waiting. Deliberately not
    /// [`StoreError::NotFound`]: the caller is owed the difference between
    /// "that never existed" and "your work was rolled back and here is why".
    Expired(String),
    /// The caller sent SQL that the store will not run against a tenant's
    /// file at all - `ATTACH`, `VACUUM`, a PRAGMA outside the allowlist. Not
    /// [`StoreError::InvalidArgument`], which is a request the store cannot
    /// act on: this one is understood perfectly well and declined. See
    /// [`crate::database`] for what is allowed and why.
    Refused(String),
    /// SQLite refused or failed.
    Sqlite(rusqlite::Error),
    /// The filesystem refused or failed - or an inbound stream broke, which
    /// arrives here as a `BrokenPipe`, because to this layer the difference
    /// between "the disk went away" and "the peer went away" is only which
    /// half of the copy stopped.
    Io(std::io::Error),
    /// A tokio blocking task panicked or was cancelled. Nothing to do with a
    /// Cloudflare Worker: this service never runs application code.
    BlockingTask(String),
    /// The peer stopped sending in the middle of a streamed request and did
    /// not close it either. Whatever it had sent is discarded.
    Stalled(String),
    /// The data directory was written before storage was keyed by tenant, so
    /// nothing in it can be read back. Startup only; see [`crate::legacy`].
    LegacyData(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tenant(m) => write!(f, "tenant: {m}"),
            Self::Unauthenticated(m) => write!(f, "unauthenticated: {m}"),
            Self::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
            Self::NotFound(m) => write!(f, "not found: {m}"),
            Self::Corrupt(m) => write!(f, "corrupt: {m}"),
            Self::TooLarge(m) => write!(f, "too large: {m}"),
            Self::RangeNotSatisfiable(m) => write!(f, "range not satisfiable: {m}"),
            Self::Exhausted(m) => write!(f, "at capacity: {m}"),
            Self::Expired(m) => write!(f, "expired: {m}"),
            Self::Refused(m) => write!(f, "refused: {m}"),
            Self::Sqlite(e) => write!(f, "sqlite: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::BlockingTask(m) => write!(f, "blocking task: {m}"),
            Self::Stalled(m) => write!(f, "stalled: {m}"),
            Self::LegacyData(m) => write!(f, "data predates the tenant contract: {m}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<tokio::task::JoinError> for StoreError {
    fn from(e: tokio::task::JoinError) -> Self {
        Self::BlockingTask(e.to_string())
    }
}

impl From<StoreError> for Status {
    fn from(e: StoreError) -> Self {
        let code = match &e {
            StoreError::Tenant(_) => Code::PermissionDenied,
            // Unauthenticated rather than PermissionDenied: the caller
            // presented no usable credential, so what it should do is obtain
            // one, not ask for wider access with the one it has.
            StoreError::Unauthenticated(_) => Code::Unauthenticated,
            StoreError::InvalidArgument(_) => Code::InvalidArgument,
            // The caller is who it says it is and the statement parses; it
            // is simply not something a tenant may run. Same code as naming
            // another tenant's handle, and for the same reason: retrying it
            // or getting a fresh token changes nothing.
            StoreError::Refused(_) => Code::PermissionDenied,
            StoreError::NotFound(_) => Code::NotFound,
            // The row exists but the bytes it points at do not: the caller
            // will never get this object back without operator repair.
            StoreError::Corrupt(_) => Code::DataLoss,
            // Same code as TooLarge and for the same reason - the server
            // will not spend the resource - but a different cause: this one
            // is a count that other callers are holding, not a size the
            // caller chose, so the answer to it is to wait rather than to
            // send something smaller.
            StoreError::TooLarge(_) | StoreError::Exhausted(_) => Code::ResourceExhausted,
            // Its own code, and the only user of it: an application forwarding
            // a `Range` header has to answer 416 for this and 404 for a
            // missing key, and it can only do that if the two arrive as
            // different codes rather than as two spellings of one.
            StoreError::RangeNotSatisfiable(_) => Code::OutOfRange,
            // The caller ran out of time, not the store: telling it apart
            // from an internal fault is what lets a client retry instead of
            // paging whoever owns the store. `Expired` is the same story from
            // the server's side of a handle it closed on a deadline.
            StoreError::Stalled(_) | StoreError::Expired(_) => Code::DeadlineExceeded,
            // Nothing the caller sent is wrong and no retry helps: the
            // service is standing on data it cannot use, and an operator has
            // to move it aside first.
            StoreError::LegacyData(_) => Code::FailedPrecondition,
            StoreError::Sqlite(_) | StoreError::Io(_) | StoreError::BlockingTask(_) => {
                Code::Internal
            }
        };
        Status::new(code, e.to_string())
    }
}

/// Convenience alias for the storage layer.
pub type StoreResult<T> = Result<T, StoreError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_match_error_kind() {
        let cases: Vec<(StoreError, Code)> = vec![
            (StoreError::Tenant("x".into()), Code::PermissionDenied),
            (
                StoreError::Unauthenticated("x".into()),
                Code::Unauthenticated,
            ),
            (
                StoreError::InvalidArgument("x".into()),
                Code::InvalidArgument,
            ),
            (StoreError::NotFound("x".into()), Code::NotFound),
            (StoreError::Corrupt("x".into()), Code::DataLoss),
            (StoreError::TooLarge("x".into()), Code::ResourceExhausted),
            (
                StoreError::RangeNotSatisfiable("x".into()),
                Code::OutOfRange,
            ),
            (StoreError::Exhausted("x".into()), Code::ResourceExhausted),
            (StoreError::Stalled("x".into()), Code::DeadlineExceeded),
            (StoreError::Expired("x".into()), Code::DeadlineExceeded),
            (StoreError::Refused("x".into()), Code::PermissionDenied),
            (
                StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows),
                Code::Internal,
            ),
            (StoreError::BlockingTask("x".into()), Code::Internal),
            (StoreError::LegacyData("x".into()), Code::FailedPrecondition),
        ];
        for (err, want) in cases {
            let status: Status = err.into();
            assert_eq!(status.code(), want);
        }
    }

    #[test]
    fn display_keeps_the_underlying_cause() {
        let e = StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "no write bit",
        ));
        assert!(e.to_string().contains("no write bit"));
    }
}
