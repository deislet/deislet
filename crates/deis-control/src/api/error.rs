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

pub type ApiResult<T> = Result<T, ApiError>;
