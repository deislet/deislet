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

/// Map product model failures onto HTTP status classes. The variants were
/// chosen so this stays a straight table with no guessing.
impl From<crate::product::ProductError> for ApiError {
    fn from(err: crate::product::ProductError) -> Self {
        use crate::product::ProductError;
        let message = err.to_string();
        match err {
            ProductError::NotFound(_) => ApiError::NotFound(message),
            ProductError::Conflict(_) => ApiError::Conflict(message),
            ProductError::Invalid(_) => ApiError::BadRequest(message),
            ProductError::BundleStorage(_) => ApiError::InternalError(message),
            ProductError::SecretsUnavailable(_) => ApiError::ServiceUnavailable(message),
            ProductError::Database(_) => ApiError::DatabaseError(message),
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
