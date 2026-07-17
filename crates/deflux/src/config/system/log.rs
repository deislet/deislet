use serde::{Deserialize, Serialize};
use std::fmt::{self, Display};
use std::str::FromStr;

use crate::config::{ConfigError, Validatable};

/// Logging level enumeration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LogLevel {
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "warn")]
    Warn,
    #[serde(rename = "info")]
    Info,
    #[serde(rename = "debug")]
    Debug,
    #[serde(rename = "trace")]
    Trace,
}

impl FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "error" => Ok(LogLevel::Error),
            "warn" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            _ => Err(format!("Unsupported log level: {s}")),
        }
    }
}

impl Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogLevel::Error => write!(f, "error"),
            LogLevel::Warn => write!(f, "warn"),
            LogLevel::Info => write!(f, "info"),
            LogLevel::Debug => write!(f, "debug"),
            LogLevel::Trace => write!(f, "trace"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PidConfig {
    #[serde(rename = "pid")]
    pub pid_file: String,
}

impl Default for PidConfig {
    fn default() -> Self {
        Self { pid_file: String::from("/var/run/deflux.pid") }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorLogConfig {
    pub level: LogLevel,
    pub path: String,
}

impl Default for ErrorLogConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            path: String::from("/var/log/deflux.log"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccessLogConfig {
    pub format: AccessLogKeys,
    pub path: String,
}

impl Default for AccessLogConfig {
    fn default() -> Self {
        Self {
            format: AccessLogKeys(vec![]),
            path: String::from("/var/log/deflux.log"),
        }
    }
}

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoggingConfig {
    pub pid: Option<PidConfig>,
    pub error_log: Option<ErrorLogConfig>,
    pub access_log: Option<AccessLogConfig>,
}

impl Validatable for LoggingConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        Ok(())
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            pid: Some(PidConfig::default()),
            error_log: Some(ErrorLogConfig::default()),
            access_log: Some(AccessLogConfig::default()),
        }
    }
}

/// Access log key types
/// Client information: remote_addr, user_agent, referer
/// Request information: time_local, method, uri, protocol
/// Response information: status, body_bytes_sent, request_time, cache_status
/// Other information: upstream_addr, ssl_protocol, ssl_cipher
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AccessLogK {
    RemoteAddr,
    UserAgent,
    Referer,
    TimeLocal,
    Method,
    Uri,
    Protocol,
    Status,
    BodyBytesSent,
    RequestTime,
    CacheStatus,
    UpstreamAddr,
    SslProtocol,
    SslCipher,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccessLogKeys(Vec<AccessLogK>);

impl AccessLogKeys {
    pub fn new(keys: Vec<AccessLogK>) -> Self {
        Self(keys)
    }
}

impl FromStr for AccessLogKeys {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s
            .split('$')
            .filter(|part| !part.is_empty())
            .map(|part| part.trim())
            .collect();
        let mut result = Vec::new();

        for part in parts {
            match part {
                "remote_addr" => result.push(AccessLogK::RemoteAddr),
                "user_agent" => result.push(AccessLogK::UserAgent),
                "referer" => result.push(AccessLogK::Referer),
                "time_local" => result.push(AccessLogK::TimeLocal),
                "method" => result.push(AccessLogK::Method),
                "uri" => result.push(AccessLogK::Uri),
                "protocol" => result.push(AccessLogK::Protocol),
                "status" => result.push(AccessLogK::Status),
                "body_bytes_sent" => result.push(AccessLogK::BodyBytesSent),
                "request_time" => result.push(AccessLogK::RequestTime),
                "cache_status" => result.push(AccessLogK::CacheStatus),
                "upstream_addr" => result.push(AccessLogK::UpstreamAddr),
                "ssl_protocol" => result.push(AccessLogK::SslProtocol),
                "ssl_cipher" => result.push(AccessLogK::SslCipher),
                _ => return Err(format!("Invalid access log key: {part}")),
            }
        }

        Ok(AccessLogKeys(result))
    }
}
