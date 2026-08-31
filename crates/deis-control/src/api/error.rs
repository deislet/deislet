use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// API error type
#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    BadRequest(String),
    /// The request is well formed but clashes with existing state, for
    /// instance creating an app that already exists.
    Conflict(String),
    /// A dependency this endpoint needs is not configured or not usable,
    /// for instance the secret master key. Retrying without fixing the
    /// control plane will not help, but the request itself was fine.
    ServiceUnavailable(String),
    /// This control plane is deliberately not set up to do what was asked -
    /// a storage token with no `storage_auth_secret`, say.
    ///
    /// Deliberately 4xx and not [`Self::ServiceUnavailable`]: nothing here is
    /// unavailable or failing, and a 5xx tells every client and every proxy
    /// in between to retry something that will answer the same way forever.
    /// The message names the configuration key, because the caller is the
    /// one who has to go and ask for it to be set.
    NotConfigured(String),
    InternalError(String),
    DatabaseError(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, msg),
            ApiError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            ApiError::NotConfigured(msg) => (StatusCode::CONFLICT, msg),
            ApiError::InternalError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            ApiError::DatabaseError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database error: {}", msg),
            ),
        };

        let body = Json(json!({
            "error": message,
        }));

        (status, body).into_response()
    }
}

impl From<crate::database::DatabaseError> for ApiError {
    fn from(err: crate::database::DatabaseError) -> Self {
        ApiError::DatabaseError(err.to_string())
    }
}

/// Map catalog failures onto HTTP status classes. The variants were
/// chosen so this stays a straight table with no guessing.
impl From<crate::catalog::CatalogError> for ApiError {
    fn from(err: crate::catalog::CatalogError) -> Self {
        use crate::catalog::CatalogError;
        let message = err.to_string();
        match err {
            CatalogError::NotFound(_) => ApiError::NotFound(message),
            CatalogError::Conflict(_) => ApiError::Conflict(message),
            CatalogError::Invalid(_) => ApiError::BadRequest(message),
            CatalogError::BundleStorage(_) => ApiError::InternalError(message),
            CatalogError::SecretsUnavailable(_) => ApiError::ServiceUnavailable(message),
            CatalogError::Database(_) | CatalogError::Backend(_) => {
                ApiError::DatabaseError(message)
            }
        }
    }
}

/// Map configuration-push failures onto HTTP status classes.
///
/// The same reasoning as the command mapping below: a push travels down a
/// control channel the node is holding open, so a node that is not connected,
/// or a configuration key that was never stored, is the caller naming
/// something that is not there rather than a fault in this process. Both used
/// to come back as 500.
impl From<crate::config_manager::PushError> for ApiError {
    fn from(err: crate::config_manager::PushError) -> Self {
        use crate::config_manager::PushError;
        let message = err.to_string();
        match err {
            PushError::NodeNotFound(_) => ApiError::NotFound(format!(
                "{message} - no node by that name is holding a control channel open, so nothing \
                 can be pushed to it. A deis-runtime node never holds one: its configuration \
                 travels with its deployment"
            )),
            PushError::ConfigNotFound(_) => ApiError::NotFound(message),
            // The channel was there when the send began and went away
            // underneath; the caller can retry, this instance cannot fix it.
            PushError::SendError(_) => ApiError::ServiceUnavailable(message),
            PushError::SerializationError(_) => ApiError::InternalError(message),
        }
    }
}

/// Map command failures onto HTTP status classes.
///
/// The one that matters is `NodeNotFound`: a command can only travel down a
/// control channel the node is holding open, so asking to command a node
/// that is not connected is the caller naming something that is not there.
/// Reporting that as a 500 blamed the control plane for a fact about the
/// fleet, and the dashboard showed it as a server failure.
impl From<crate::command::ExecuteError> for ApiError {
    fn from(err: crate::command::ExecuteError) -> Self {
        use crate::command::ExecuteError;
        let message = err.to_string();
        match err {
            ExecuteError::NodeNotFound(_) => ApiError::NotFound(format!(
                "{message} - no node by that name is holding a control channel open, so nothing can be sent to it"
            )),
            ExecuteError::CommandNotFound => ApiError::NotFound(message),
            ExecuteError::InvalidTimeout => ApiError::BadRequest(message),
            ExecuteError::DatabaseError(_) => ApiError::DatabaseError(message),
            // The node was there when the send began and its channel went
            // away underneath: the caller can retry, this instance cannot
            // fix it.
            ExecuteError::SendError(_) | ExecuteError::ChannelClosed => {
                ApiError::ServiceUnavailable(message)
            }
            ExecuteError::SerializationError(_) | ExecuteError::Timeout => {
                ApiError::InternalError(message)
            }
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
