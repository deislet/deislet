use serde::{Deserialize, Serialize};
use std::fmt;

pub type SdkResult<T> = Result<T, WireError>;

/// SDK error types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireError {
    /// Configuration error
    ConfigurationError(String),
    /// Authentication error
    AuthenticationError(String),
    /// Network error
    NetworkError(String),
    /// Control returned an error
    ControlError {
        code: String,
        message: String,
        help: Option<String>,
    },
    /// Instance status error
    InstanceStatusError(String),
    /// Serialization error
    SerializationError(String),
    /// Connection error
    ConnectionError(String),
    /// Registration error
    RegistrationError(String),
    /// Communication error
    CommunicationError(String),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::ConfigurationError(msg) => write!(f, "Configuration error: {msg}"),
            WireError::AuthenticationError(msg) => write!(f, "Authentication error: {msg}"),
            WireError::NetworkError(msg) => write!(f, "Network error: {msg}"),
            WireError::ControlError {
                code,
                message,
                help,
            } => {
                if let Some(help_text) = help {
                    write!(
                        f,
                        "Control error [{code}]: {message} (help: {help_text})"
                    )
                } else {
                    write!(f, "Control error [{code}]: {message}")
                }
            }
            WireError::InstanceStatusError(msg) => write!(f, "Instance status error: {msg}"),
            WireError::SerializationError(msg) => write!(f, "Serialization error: {msg}"),
            WireError::ConnectionError(msg) => write!(f, "Connection error: {msg}"),
            WireError::RegistrationError(msg) => write!(f, "Registration error: {msg}"),
            WireError::CommunicationError(msg) => write!(f, "Communication error: {msg}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<serde_json::Error> for WireError {
    fn from(err: serde_json::Error) -> Self {
        WireError::SerializationError(err.to_string())
    }
}

impl From<String> for WireError {
    fn from(err: String) -> Self {
        WireError::ConfigurationError(err)
    }
}

/// Control error response structure
#[derive(Debug, Deserialize, Serialize)]
pub struct ControlErrorResponse {
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
        let error = WireError::ConfigurationError("Invalid config".to_string());
        assert_eq!(error.to_string(), "Configuration error: Invalid config");
    }

    #[test]
    fn test_authentication_error_display() {
        let error = WireError::AuthenticationError("Invalid credentials".to_string());
        assert_eq!(
            error.to_string(),
            "Authentication error: Invalid credentials"
        );
    }

    #[test]
    fn test_network_error_display() {
        let error = WireError::NetworkError("Connection failed".to_string());
        assert_eq!(error.to_string(), "Network error: Connection failed");
    }

    #[test]
    fn test_control_error_display_with_help() {
        let error = WireError::ControlError {
            code: "INVALID_REQUEST".to_string(),
            message: "Bad request".to_string(),
            help: Some("Check your parameters".to_string()),
        };
        assert_eq!(
            error.to_string(),
            "Control error [INVALID_REQUEST]: Bad request (help: Check your parameters)"
        );
    }

    #[test]
    fn test_control_error_display_without_help() {
        let error = WireError::ControlError {
            code: "SERVER_ERROR".to_string(),
            message: "Internal server error".to_string(),
            help: None,
        };
        assert_eq!(
            error.to_string(),
            "Control error [SERVER_ERROR]: Internal server error"
        );
    }

    #[test]
    fn test_instance_status_error_display() {
        let error = WireError::InstanceStatusError("Invalid transition".to_string());
        assert_eq!(
            error.to_string(),
            "Instance status error: Invalid transition"
        );
    }

    #[test]
    fn test_serialization_error_display() {
        let error = WireError::SerializationError("JSON parse error".to_string());
        assert_eq!(error.to_string(), "Serialization error: JSON parse error");
    }

    #[test]
    fn test_from_serde_json_error() {
        let json_error = serde_json::from_str::<serde_json::Value>("invalid json").unwrap_err();
        let sdk_error = WireError::from(json_error);

        match sdk_error {
            WireError::SerializationError(msg) => assert!(msg.contains("expected")),
            _ => panic!("Expected SerializationError"),
        }
    }

    #[test]
    fn test_error_trait_implementation() {
        let error = WireError::ConfigurationError("test".to_string());
        let _: &dyn std::error::Error = &error;
    }

    #[test]
    fn test_control_error_response_serialization() {
        let response = ControlErrorResponse {
            success: false,
            error: "INVALID_REQUEST".to_string(),
            message: "Bad request".to_string(),
            help: Some("Check parameters".to_string()),
        };

        let json = serde_json::to_string(&response).unwrap();
        let deserialized: ControlErrorResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(response.success, deserialized.success);
        assert_eq!(response.error, deserialized.error);
        assert_eq!(response.message, deserialized.message);
        assert_eq!(response.help, deserialized.help);
    }

    #[test]
    fn test_sdk_error_clone() {
        let error = WireError::ConfigurationError("test".to_string());
        let cloned = error.clone();

        match (error, cloned) {
            (WireError::ConfigurationError(msg1), WireError::ConfigurationError(msg2)) => {
                assert_eq!(msg1, msg2);
            }
            _ => panic!("Clone failed"),
        }
    }

    #[test]
    fn test_sdk_error_debug() {
        let error = WireError::NetworkError("test".to_string());
        let debug_string = format!("{error:?}");
        assert!(debug_string.contains("NetworkError"));
        assert!(debug_string.contains("test"));
    }

    #[test]
    fn test_sdk_result_type_alias() {
        fn returns_result() -> SdkResult<i32> {
            Ok(42)
        }

        fn returns_error() -> SdkResult<i32> {
            Err(WireError::ConfigurationError("test".to_string()))
        }

        assert_eq!(returns_result().unwrap(), 42);
        assert!(returns_error().is_err());
    }
}
