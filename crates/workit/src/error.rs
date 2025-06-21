use serde::{Deserialize, Serialize};
use std::fmt;

pub type SdkResult<T> = Result<T, WorkitError>;

/// SDK error types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkitError {
    /// Configuration error
    ConfigurationError(String),
    /// Authentication error
    AuthenticationError(String),
    /// Network error
    NetworkError(String),
    /// Supervisor returned an error
    SupervisorError {
        code: String,
        message: String,
        help: Option<String>,
    },
    /// Instance status error
    InstanceStatusError(String),
    /// Serialization error
    SerializationError(String),
}

impl fmt::Display for WorkitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkitError::ConfigurationError(msg) => write!(f, "Configuration error: {msg}"),
            WorkitError::AuthenticationError(msg) => write!(f, "Authentication error: {msg}"),
            WorkitError::NetworkError(msg) => write!(f, "Network error: {msg}"),
            WorkitError::SupervisorError {
                code,
                message,
                help,
            } => {
                if let Some(help_text) = help {
                    write!(
                        f,
                        "Supervisor error [{code}]: {message} (help: {help_text})"
                    )
                } else {
                    write!(f, "Supervisor error [{code}]: {message}")
                }
            }
            WorkitError::InstanceStatusError(msg) => write!(f, "Instance status error: {msg}"),
            WorkitError::SerializationError(msg) => write!(f, "Serialization error: {msg}"),
        }
    }
}

impl std::error::Error for WorkitError {}

impl From<reqwest::Error> for WorkitError {
    fn from(err: reqwest::Error) -> Self {
        WorkitError::NetworkError(err.to_string())
    }
}

impl From<serde_json::Error> for WorkitError {
    fn from(err: serde_json::Error) -> Self {
        WorkitError::SerializationError(err.to_string())
    }
}

/// Supervisor error response structure
#[derive(Debug, Deserialize, Serialize)]
pub struct SupervisorErrorResponse {
    pub success: bool,
    pub error: String,
    pub message: String,
    pub help: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_configuration_error_display() {
        let error = WorkitError::ConfigurationError("Invalid config".to_string());
        assert_eq!(error.to_string(), "Configuration error: Invalid config");
    }

    #[test]
    fn test_authentication_error_display() {
        let error = WorkitError::AuthenticationError("Invalid credentials".to_string());
        assert_eq!(
            error.to_string(),
            "Authentication error: Invalid credentials"
        );
    }

    #[test]
    fn test_network_error_display() {
        let error = WorkitError::NetworkError("Connection failed".to_string());
        assert_eq!(error.to_string(), "Network error: Connection failed");
    }

    #[test]
    fn test_supervisor_error_display_with_help() {
        let error = WorkitError::SupervisorError {
            code: "INVALID_REQUEST".to_string(),
            message: "Bad request".to_string(),
            help: Some("Check your parameters".to_string()),
        };
        assert_eq!(
            error.to_string(),
            "Supervisor error [INVALID_REQUEST]: Bad request (help: Check your parameters)"
        );
    }

    #[test]
    fn test_supervisor_error_display_without_help() {
        let error = WorkitError::SupervisorError {
            code: "SERVER_ERROR".to_string(),
            message: "Internal server error".to_string(),
            help: None,
        };
        assert_eq!(
            error.to_string(),
            "Supervisor error [SERVER_ERROR]: Internal server error"
        );
    }

    #[test]
    fn test_instance_status_error_display() {
        let error = WorkitError::InstanceStatusError("Invalid transition".to_string());
        assert_eq!(
            error.to_string(),
            "Instance status error: Invalid transition"
        );
    }

    #[test]
    fn test_serialization_error_display() {
        let error = WorkitError::SerializationError("JSON parse error".to_string());
        assert_eq!(error.to_string(), "Serialization error: JSON parse error");
    }

    #[test]
    fn test_from_reqwest_error() {
        // Test the conversion trait by creating a mock error
        // Since we can't easily create a reqwest::Error, we'll test the trait implementation
        // by using a different approach

        // This test verifies that the From<reqwest::Error> trait is implemented
        fn test_conversion(error: reqwest::Error) -> WorkitError {
            WorkitError::from(error)
        }

        // We can't easily create a reqwest::Error in tests, so let's just verify
        // the trait exists by checking the function compiles
        let _test_fn: fn(reqwest::Error) -> WorkitError = test_conversion;
    }

    #[test]
    fn test_from_serde_json_error() {
        let json_error = serde_json::from_str::<serde_json::Value>("invalid json").unwrap_err();
        let sdk_error = WorkitError::from(json_error);

        match sdk_error {
            WorkitError::SerializationError(msg) => assert!(msg.contains("expected")),
            _ => panic!("Expected SerializationError"),
        }
    }

    #[test]
    fn test_error_trait_implementation() {
        let error = WorkitError::ConfigurationError("test".to_string());
        let _: &dyn std::error::Error = &error;
    }

    #[test]
    fn test_supervisor_error_response_serialization() {
        let response = SupervisorErrorResponse {
            success: false,
            error: "INVALID_REQUEST".to_string(),
            message: "Bad request".to_string(),
            help: Some("Check parameters".to_string()),
        };

        let json = serde_json::to_string(&response).unwrap();
        let deserialized: SupervisorErrorResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(response.success, deserialized.success);
        assert_eq!(response.error, deserialized.error);
        assert_eq!(response.message, deserialized.message);
        assert_eq!(response.help, deserialized.help);
    }

    #[test]
    fn test_sdk_error_clone() {
        let error = WorkitError::ConfigurationError("test".to_string());
        let cloned = error.clone();

        match (error, cloned) {
            (WorkitError::ConfigurationError(msg1), WorkitError::ConfigurationError(msg2)) => {
                assert_eq!(msg1, msg2);
            }
            _ => panic!("Clone failed"),
        }
    }

    #[test]
    fn test_sdk_error_debug() {
        let error = WorkitError::NetworkError("test".to_string());
        let debug_string = format!("{:?}", error);
        assert!(debug_string.contains("NetworkError"));
        assert!(debug_string.contains("test"));
    }

    #[test]
    fn test_sdk_result_type_alias() {
        fn returns_result() -> SdkResult<i32> {
            Ok(42)
        }

        fn returns_error() -> SdkResult<i32> {
            Err(WorkitError::ConfigurationError("test".to_string()))
        }

        assert_eq!(returns_result().unwrap(), 42);
        assert!(returns_error().is_err());
    }
}
