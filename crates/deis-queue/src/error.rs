//! One error type for every queue path, plus the mapping to gRPC status.
//!
//! Refusals arrive at the caller as a gRPC status rather than as a flag in
//! the response body. `SendResponse` does carry `success` and `error`, but
//! they are there for wire compatibility with the producer client that
//! already exists; a send this service will not perform fails the call, so a
//! caller cannot mistake a refusal for a stored message by ignoring a field.

use std::fmt;
use tonic::{Code, Status};

/// Anything that can go wrong below the gRPC layer.
#[derive(Debug)]
pub enum QueueError {
    /// The caller did not identify a usable tenant.
    Tenant(String),
    /// The caller did not prove it may act as the tenant it named. Distinct
    /// from [`QueueError::Tenant`], which is a tenant id the service cannot
    /// use at all: this one is well formed and simply not the caller's to
    /// name. See [`crate::auth`].
    Unauthenticated(String),
    /// The caller sent a request the queue cannot act on at all.
    InvalidArgument(String),
    /// The request exceeds a configured ceiling.
    TooLarge(String),
    /// A stored message cannot be handed back as it was stored.
    Corrupt(String),
    /// A migration coordinator has durably fenced this tenant's mutations.
    WriteFenced(String),
    /// SQLite refused or failed.
    Sqlite(rusqlite::Error),
    /// The filesystem refused or failed.
    Io(std::io::Error),
    /// A tokio blocking task panicked or was cancelled. Nothing to do with a
    /// Cloudflare Worker: this service never runs application code.
    BlockingTask(String),
    /// The data directory was written before the queue was keyed by tenant,
    /// so nothing in it can be delivered. Startup only; see [`crate::legacy`].
    LegacyData(String),
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tenant(m) => write!(f, "tenant: {m}"),
            Self::Unauthenticated(m) => write!(f, "unauthenticated: {m}"),
            Self::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
            Self::TooLarge(m) => write!(f, "too large: {m}"),
            Self::Corrupt(m) => write!(f, "corrupt: {m}"),
            Self::WriteFenced(m) => write!(f, "write fenced: {m}"),
            Self::Sqlite(e) => write!(f, "sqlite: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::BlockingTask(m) => write!(f, "blocking task: {m}"),
            Self::LegacyData(m) => write!(f, "data predates the tenant contract: {m}"),
        }
    }
}

impl std::error::Error for QueueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for QueueError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<std::io::Error> for QueueError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<tokio::task::JoinError> for QueueError {
    fn from(e: tokio::task::JoinError) -> Self {
        Self::BlockingTask(e.to_string())
    }
}

impl From<deis_backend_migration::FenceError> for QueueError {
    fn from(error: deis_backend_migration::FenceError) -> Self {
        match error {
            deis_backend_migration::FenceError::Fenced(receipt) => Self::WriteFenced(format!(
                "tenant {:?} is frozen by migration {} at fence epoch {}; retry only after the migration coordinator releases or cuts over the namespace",
                receipt.tenant, receipt.migration_id, receipt.fence_epoch
            )),
            other => Self::BlockingTask(other.to_string()),
        }
    }
}

impl From<QueueError> for Status {
    fn from(e: QueueError) -> Self {
        let code = match &e {
            QueueError::Tenant(_) => Code::PermissionDenied,
            // Unauthenticated rather than PermissionDenied: the caller
            // presented no usable credential, so what it should do is obtain
            // one, not ask for wider access with the one it has.
            QueueError::Unauthenticated(_) => Code::Unauthenticated,
            QueueError::InvalidArgument(_) => Code::InvalidArgument,
            QueueError::TooLarge(_) => Code::ResourceExhausted,
            // The row is there but cannot be handed back: no retry by the
            // caller will fix it, an operator has to.
            QueueError::Corrupt(_) => Code::DataLoss,
            QueueError::WriteFenced(_) => Code::FailedPrecondition,
            // Nothing the caller sent is wrong and no retry helps: the
            // service is standing on data it cannot use, and an operator has
            // to move it aside first.
            QueueError::LegacyData(_) => Code::FailedPrecondition,
            QueueError::Sqlite(_) | QueueError::Io(_) | QueueError::BlockingTask(_) => {
                Code::Internal
            }
        };
        Status::new(code, e.to_string())
    }
}

/// Convenience alias for the queue layer.
pub type QueueResult<T> = Result<T, QueueError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_match_error_kind() {
        let cases: Vec<(QueueError, Code)> = vec![
            (QueueError::Tenant("x".into()), Code::PermissionDenied),
            (
                QueueError::Unauthenticated("x".into()),
                Code::Unauthenticated,
            ),
            (
                QueueError::InvalidArgument("x".into()),
                Code::InvalidArgument,
            ),
            (QueueError::TooLarge("x".into()), Code::ResourceExhausted),
            (QueueError::Corrupt("x".into()), Code::DataLoss),
            (
                QueueError::WriteFenced("x".into()),
                Code::FailedPrecondition,
            ),
            (
                QueueError::Sqlite(rusqlite::Error::QueryReturnedNoRows),
                Code::Internal,
            ),
            (QueueError::BlockingTask("x".into()), Code::Internal),
            (QueueError::LegacyData("x".into()), Code::FailedPrecondition),
        ];
        for (err, want) in cases {
            let status: Status = err.into();
            assert_eq!(status.code(), want);
        }
    }

    #[test]
    fn display_keeps_the_underlying_cause() {
        let e = QueueError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "no write bit",
        ));
        assert!(e.to_string().contains("no write bit"));
    }
}
