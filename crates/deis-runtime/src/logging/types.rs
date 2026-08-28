use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

/// Log level
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
            LogLevel::Fatal => "fatal",
        }
    }

    pub fn as_i32(&self) -> i32 {
        match self {
            LogLevel::Trace => 1,
            LogLevel::Debug => 2,
            LogLevel::Info => 3,
            LogLevel::Warn => 4,
            LogLevel::Error => 5,
            LogLevel::Fatal => 6,
        }
    }
}

impl From<tracing::Level> for LogLevel {
    fn from(level: tracing::Level) -> Self {
        match level {
            tracing::Level::TRACE => LogLevel::Trace,
            tracing::Level::DEBUG => LogLevel::Debug,
            tracing::Level::INFO => LogLevel::Info,
            tracing::Level::WARN => LogLevel::Warn,
            tracing::Level::ERROR => LogLevel::Error,
        }
    }
}

/// Log entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Timestamp
    pub timestamp: SystemTime,

    /// Log level
    pub level: LogLevel,

    /// Log source (worker ID)
    pub source: String,

    /// Application ID (optional)
    pub app_id: Option<String>,

    /// Log message
    pub message: String,

    /// Structured fields
    pub fields: HashMap<String, String>,

    /// Trace ID (for distributed tracing)
    pub trace_id: Option<String>,

    /// Span ID
    pub span_id: Option<String>,
}

impl LogEntry {
    pub fn new(source: String, level: LogLevel, message: String) -> Self {
        Self {
            timestamp: SystemTime::now(),
            level,
            source,
            app_id: None,
            message,
            fields: HashMap::new(),
            trace_id: None,
            span_id: None,
        }
    }

    pub fn with_app_id(mut self, app_id: String) -> Self {
        self.app_id = Some(app_id);
        self
    }

    pub fn with_field(mut self, key: String, value: String) -> Self {
        self.fields.insert(key, value);
        self
    }

    pub fn with_trace(mut self, trace_id: String, span_id: String) -> Self {
        self.trace_id = Some(trace_id);
        self.span_id = Some(span_id);
        self
    }
}
