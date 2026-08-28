//! Per-request access log.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

use anyhow::{Context, Result};
use pingora::http::RequestHeader;
use pingora::proxy::Session;
use tracing::warn;

use super::RequestCtx;
use crate::config::system::{AccessLogConfig, AccessLogK};

/// Appends one line per request to a file, in the configured field order.
pub struct AccessLog {
    keys: Vec<AccessLogK>,
    path: String,
    file: Mutex<File>,
}

impl AccessLog {
    pub fn open(config: &AccessLogConfig) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.path)
            .with_context(|| format!("opening access log {}", config.path))?;
        Ok(Self {
            keys: config.format.keys().to_vec(),
            path: config.path.clone(),
            file: Mutex::new(file),
        })
    }

    pub fn record(&self, session: &Session, ctx: &RequestCtx) {
        let line = self.format(session, ctx);
        match self.file.lock() {
            Ok(mut file) => {
                if let Err(e) = writeln!(file, "{line}") {
                    warn!(path = %self.path, error = %e, "access log write failed");
                }
            }
            Err(_) => warn!(path = %self.path, "access log lock poisoned; entry dropped"),
        }
    }

    fn format(&self, session: &Session, ctx: &RequestCtx) -> String {
        let req = session.req_header();
        let ssl = session.digest().and_then(|d| d.ssl_digest.as_ref());
        let mut line = String::new();
        for (index, key) in self.keys.iter().enumerate() {
            if index > 0 {
                line.push(' ');
            }
            let value = match key {
                AccessLogK::RemoteAddr => session.client_addr().map(|addr| addr.to_string()),
                AccessLogK::UserAgent => header(req, "user-agent"),
                AccessLogK::Referer => header(req, "referer"),
                AccessLogK::TimeLocal => Some(chrono::Local::now().to_rfc3339()),
                AccessLogK::Host => Some(ctx.host.clone()),
                AccessLogK::AppId => ctx.app_id.clone(),
                AccessLogK::Method => Some(req.method.to_string()),
                AccessLogK::Uri => req.uri.path_and_query().map(|p| p.to_string()),
                AccessLogK::Protocol => Some(
                    if session.is_http2() {
                        "HTTP/2"
                    } else {
                        "HTTP/1.1"
                    }
                    .to_string(),
                ),
                AccessLogK::Status => session
                    .response_written()
                    .map(|resp| resp.status.as_u16().to_string()),
                AccessLogK::BodyBytesSent => Some(session.body_bytes_sent().to_string()),
                AccessLogK::RequestTime => Some(format!("{:.3}", ctx.elapsed().as_secs_f64())),
                AccessLogK::UpstreamAddr => ctx.upstream.clone(),
                AccessLogK::SslProtocol => ssl.map(|s| s.version.to_string()),
                AccessLogK::SslCipher => ssl.map(|s| s.cipher.to_string()),
            };
            line.push_str(&field(value));
        }
        line
    }
}

fn header(req: &RequestHeader, name: &str) -> Option<String> {
    req.headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Render a field value; empty values become `-`, values with spaces are
/// quoted so the line stays splittable.
fn field(value: Option<String>) -> String {
    match value {
        Some(value) if !value.is_empty() => {
            if value.contains(' ') || value.contains('"') {
                format!("\"{}\"", value.replace('"', "\\\""))
            } else {
                value
            }
        }
        _ => "-".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_missing_and_spaced_values() {
        assert_eq!(field(None), "-");
        assert_eq!(field(Some(String::new())), "-");
        assert_eq!(field(Some("plain".to_string())), "plain");
        assert_eq!(
            field(Some("Mozilla/5.0 (X11)".to_string())),
            "\"Mozilla/5.0 (X11)\""
        );
        assert_eq!(field(Some("say \"hi\"".to_string())), "\"say \\\"hi\\\"\"");
    }
}
