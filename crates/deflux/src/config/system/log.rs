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

/// Where diagnostics go. Without a path they are written to stderr.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ErrorLogConfig {
    pub level: LogLevel,
    pub path: Option<String>,
}

impl Default for ErrorLogConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            path: None,
        }
    }
}

/// One line per request, appended to `path` in the field order given by
/// `format` (for example `"$remote_addr $method $uri $status $request_time"`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccessLogConfig {
    #[serde(default)]
    pub format: AccessLogKeys,
    pub path: String,
}

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LoggingConfig {
    pub error_log: Option<ErrorLogConfig>,
    pub access_log: Option<AccessLogConfig>,
}

impl Validatable for LoggingConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(access) = &self.access_log {
            if access.path.trim().is_empty() {
                return Err(ConfigError::ValidationError {
                    message: "log.access_log.path is empty".to_string(),
                });
            }
            if access.format.keys().is_empty() {
                return Err(ConfigError::ValidationError {
                    message: "log.access_log.format has no fields".to_string(),
                });
            }
        }
        Ok(())
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            error_log: Some(ErrorLogConfig::default()),
            access_log: None,
        }
    }
}

/// Fields available to the access log format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessLogK {
    RemoteAddr,
    UserAgent,
    Referer,
    TimeLocal,
    Host,
    AppId,
    Method,
    Uri,
    Protocol,
    Status,
    BodyBytesSent,
    RequestTime,
    UpstreamAddr,
    SslProtocol,
    SslCipher,
}

impl AccessLogK {
    pub fn name(&self) -> &'static str {
        match self {
            Self::RemoteAddr => "remote_addr",
            Self::UserAgent => "user_agent",
            Self::Referer => "referer",
            Self::TimeLocal => "time_local",
            Self::Host => "host",
            Self::AppId => "app_id",
            Self::Method => "method",
            Self::Uri => "uri",
            Self::Protocol => "protocol",
            Self::Status => "status",
            Self::BodyBytesSent => "body_bytes_sent",
            Self::RequestTime => "request_time",
            Self::UpstreamAddr => "upstream_addr",
            Self::SslProtocol => "ssl_protocol",
            Self::SslCipher => "ssl_cipher",
        }
    }
}

impl FromStr for AccessLogK {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "remote_addr" => Self::RemoteAddr,
            "user_agent" => Self::UserAgent,
            "referer" => Self::Referer,
            "time_local" => Self::TimeLocal,
            "host" => Self::Host,
            "app_id" => Self::AppId,
            "method" => Self::Method,
            "uri" => Self::Uri,
            "protocol" => Self::Protocol,
            "status" => Self::Status,
            "body_bytes_sent" => Self::BodyBytesSent,
            "request_time" => Self::RequestTime,
            "upstream_addr" => Self::UpstreamAddr,
            "ssl_protocol" => Self::SslProtocol,
            "ssl_cipher" => Self::SslCipher,
            _ => return Err(format!("Invalid access log key: {s}")),
        })
    }
}

/// Ordered list of access log fields, written as `$key $key ...`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "String", into = "String")]
pub struct AccessLogKeys(Vec<AccessLogK>);

impl AccessLogKeys {
    pub fn new(keys: Vec<AccessLogK>) -> Self {
        Self(keys)
    }

    pub fn keys(&self) -> &[AccessLogK] {
        &self.0
    }
}

impl Default for AccessLogKeys {
    fn default() -> Self {
        Self(vec![
            AccessLogK::TimeLocal,
            AccessLogK::RemoteAddr,
            AccessLogK::Host,
            AccessLogK::AppId,
            AccessLogK::Method,
            AccessLogK::Uri,
            AccessLogK::Protocol,
            AccessLogK::Status,
            AccessLogK::BodyBytesSent,
            AccessLogK::RequestTime,
            AccessLogK::UpstreamAddr,
        ])
    }
}

impl FromStr for AccessLogKeys {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let keys = s
            .split('$')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(AccessLogK::from_str)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self(keys))
    }
}

impl Display for AccessLogKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for key in &self.0 {
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            write!(f, "${}", key.name())?;
        }
        Ok(())
    }
}

impl TryFrom<String> for AccessLogKeys {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<AccessLogKeys> for String {
    fn from(value: AccessLogKeys) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_format_string() {
        let keys: AccessLogKeys = "$remote_addr $method $uri $status".parse().unwrap();
        assert_eq!(
            keys.keys(),
            &[
                AccessLogK::RemoteAddr,
                AccessLogK::Method,
                AccessLogK::Uri,
                AccessLogK::Status
            ]
        );
        assert_eq!(keys.to_string(), "$remote_addr $method $uri $status");
    }

    #[test]
    fn rejects_unknown_key() {
        assert!("$remote_addr $bogus".parse::<AccessLogKeys>().is_err());
    }

    #[test]
    fn access_log_needs_path_and_fields() {
        let config: LoggingConfig = toml::from_str(
            r#"
[access_log]
path = "/tmp/access.log"
format = "$method $uri"
"#,
        )
        .unwrap();
        assert!(config.validate().is_ok());

        let config: LoggingConfig = toml::from_str(
            r#"
[access_log]
path = ""
"#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }
}
