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
    /// The caller sent a request the store cannot act on at all.
    InvalidArgument(String),
    /// The addressed object does not exist.
    NotFound(String),
    /// Metadata and content disagree: something that should be there is gone.
    Corrupt(String),
    /// The request exceeds a configured ceiling.
    TooLarge(String),
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
            Self::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
            Self::NotFound(m) => write!(f, "not found: {m}"),
            Self::Corrupt(m) => write!(f, "corrupt: {m}"),
            Self::TooLarge(m) => write!(f, "too large: {m}"),
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
            StoreError::InvalidArgument(_) => Code::InvalidArgument,
            StoreError::NotFound(_) => Code::NotFound,
            // The row exists but the bytes it points at do not: the caller
            // will never get this object back without operator repair.
            StoreError::Corrupt(_) => Code::DataLoss,
            StoreError::TooLarge(_) => Code::ResourceExhausted,
            // The caller ran out of time, not the store: telling it apart
            // from an internal fault is what lets a client retry instead of
            // paging whoever owns the store.
            StoreError::Stalled(_) => Code::DeadlineExceeded,
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
                StoreError::InvalidArgument("x".into()),
                Code::InvalidArgument,
            ),
            (StoreError::NotFound("x".into()), Code::NotFound),
            (StoreError::Corrupt("x".into()), Code::DataLoss),
            (StoreError::TooLarge("x".into()), Code::ResourceExhausted),
            (StoreError::Stalled("x".into()), Code::DeadlineExceeded),
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
