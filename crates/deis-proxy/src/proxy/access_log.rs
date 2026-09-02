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
        // User-Agent, Referer and the request path are the client's bytes.
        // This file is read by line, so a newline in any of them would write
        // a whole line the client composed, and a U+009B would repaint the
        // terminal of whoever tails it. The same rule every service's
        // `tracing` output already obeys applies here, to the whole line: a
        // legitimate field never contains a control character.
        let formatted = self.format(session, ctx);
        let line = deis_wire::escape_control(&formatted);
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
                AccessLogK::Protocol => Some(format!("{:?}", req.version)),
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
    /// The one rule `record` adds on top of `format`: whatever a field holds,
    /// the file gains exactly one line, and a control character in a field is
    /// spelled out rather than written.
    ///
    /// This drives `record` itself, through a real `Session` read off an
    /// in-memory stream, because the escape sits in `record` and not in
    /// `format` - a test of `format` alone would stay green with the escape
    /// removed. That was measured: with line 44 replaced by a borrow of the
    /// unescaped text, `cargo test -p deis-proxy --lib -- access_log` passed.
    ///
    /// What each byte stands for. A header value reaches `format` through
    /// `HeaderValue::to_str`, which admits visible ASCII and HTAB and nothing
    /// else, so a tab in User-Agent is the control character a client can
    /// put on this line today, and it is put there here the way a client
    /// would - in the request bytes. `ctx.host` and the other `String` fields
    /// are filled in by this crate from the same headers, so no producer hands
    /// them a newline or a U+009B today; the rule is about the line, not
    /// about which producer is trusted, so the forged second line and the CSI
    /// byte are given to the field a future producer is most likely to feed
    /// from bytes it did not check.
    #[tokio::test]
    async fn a_field_cannot_write_a_second_line_or_repaint_a_terminal() {
        use tokio::io::AsyncWriteExt;

        let (mut client, server) = tokio::io::duplex(4096);
        let mut session = Session::new_h1(Box::new(server));
        client
            .write_all(
                b"GET /hello HTTP/1.1\r\nHost: shop.example\r\n\
                  User-Agent: Mozilla/5.0\t(tab)\r\n\r\n",
            )
            .await
            .expect("the request bytes are written");
        assert!(
            session.read_request().await.expect("a request is parsed"),
            "the in-memory stream carried one request"
        );

        let forged = "shop.example\n2026-09-02T00:00:00Z 10.0.0.1 forged GET /admin 200\u{9b}31m";
        let ctx = RequestCtx {
            host: forged.to_string(),
            app_id: Some("shop".to_string()),
            ..Default::default()
        };

        let dir = tempfile::tempdir().expect("a directory for the log");
        let path = dir.path().join("access.log");
        let config = AccessLogConfig {
            format: crate::config::system::AccessLogKeys::new(vec![
                AccessLogK::Host,
                AccessLogK::AppId,
                AccessLogK::Method,
                AccessLogK::Uri,
                AccessLogK::UserAgent,
            ]),
            path: path.to_string_lossy().into_owned(),
        };
        let log = AccessLog::open(&config).expect("the log opens");
        log.record(&session, &ctx);
        drop(log);

        let written = std::fs::read_to_string(&path).expect("the log is readable");
        assert_eq!(
            written.matches('\n').count(),
            1,
            "one request must be one line; the file holds:\n{written}"
        );
        let line = written.trim_end_matches('\n');
        assert!(
            !line.chars().any(char::is_control),
            "no control character may reach the file: {line:?}"
        );
        assert!(
            line.contains("shop.example\\n2026-09-02T00:00:00Z 10.0.0.1 forged GET /admin 200"),
            "the newline is spelled out and the forged line stays inside the field: {line}"
        );
        assert!(
            line.contains("200\\u{9b}31m"),
            "the CSI byte is spelled out rather than written: {line}"
        );
        assert!(
            line.contains("Mozilla/5.0\\t(tab)"),
            "the tab a client can put in a header is spelled out: {line}"
        );
        assert!(
            line.contains(" shop GET /hello "),
            "the fields around the client's bytes are still the ones asked for: {line}"
        );
    }
}
