//! Reading logs back out of deis-telemetry.
//!
//! deis-telemetry knows applications, not tenants: a node ships every
//! environment of one application under the same `app_id` and puts the
//! environment in the entry's `fields` (docs/implementation-plan.zh.md
//! section 1). So `-e/--environment` is a filter this CLI applies to the
//! field, not something the service can be asked to narrow by.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tonic::transport::{Channel, Endpoint};

/// Generated client for `deis.telemetry`, compiled from the proto in
/// deis-telemetry by this crate's `build.rs`.
pub mod proto {
    tonic::include_proto!("deis.telemetry");
}

use proto::log_service_client::LogServiceClient;
pub use proto::{LogEntry, LogLevel, QueryRequest, QueryResponse, TailRequest};

/// How long dialling deis-telemetry may take before it is called unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one `Query` may take. `Tail` is deliberately not bounded: it is a
/// stream a person leaves open.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// The name of the structured field carrying the environment.
///
/// It is written by deis-runtime's tracing layer under exactly this key; a
/// disagreement here would silently filter every line away.
pub const ENVIRONMENT_FIELD: &str = "environment";

/// Severity floor for a query or a tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level(pub LogLevel);

impl FromStr for Level {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let level = match s.trim().to_ascii_lowercase().as_str() {
            "trace" => LogLevel::Trace,
            "debug" => LogLevel::Debug,
            "info" => LogLevel::Info,
            "warn" | "warning" => LogLevel::Warn,
            "error" => LogLevel::Error,
            "fatal" => LogLevel::Fatal,
            other => bail!(
                "unknown log level {other:?}; use one of trace, debug, info, warn, error, fatal"
            ),
        };
        Ok(Self(level))
    }
}

/// The name to print for one entry's level.
pub fn level_name(level: i32) -> &'static str {
    match LogLevel::try_from(level).unwrap_or(LogLevel::Unspecified) {
        LogLevel::Trace => "TRACE",
        LogLevel::Debug => "DEBUG",
        LogLevel::Info => "INFO",
        LogLevel::Warn => "WARN",
        LogLevel::Error => "ERROR",
        LogLevel::Fatal => "FATAL",
        LogLevel::Unspecified => "-",
    }
}

/// Whether an entry belongs to `environment`.
///
/// An entry that carries no environment field at all is kept: it comes from a
/// component that is not serving one particular environment, and dropping it
/// would hide exactly the lines that explain why an application is not
/// running.
pub fn entry_matches_environment(fields: &HashMap<String, String>, environment: &str) -> bool {
    match fields.get(ENVIRONMENT_FIELD) {
        Some(value) => value == environment,
        None => true,
    }
}

/// A client for one deis-telemetry.
pub struct Telemetry {
    client: LogServiceClient<Channel>,
    endpoint: String,
}

impl Telemetry {
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let url = normalise_endpoint(endpoint);
        let channel = Endpoint::from_shared(url.clone())
            .with_context(|| format!("{endpoint} is not a usable address for deis-telemetry"))?
            .connect_timeout(CONNECT_TIMEOUT)
            .connect()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "could not reach deis-telemetry at {url}: {e}\nCheck that it is running and \
                     that the address is right (see `deis config list`)."
                )
            })?;

        let client = LogServiceClient::new(channel)
            .max_decoding_message_size(deis_wire::conventions::MAX_GRPC_MESSAGE_BYTES);
        Ok(Self {
            client,
            endpoint: url,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// One page of stored entries, newest first.
    pub async fn query(&mut self, request: QueryRequest) -> Result<QueryResponse> {
        let mut request = tonic::Request::new(request);
        request.set_timeout(QUERY_TIMEOUT);
        let response = self.client.query(request).await.map_err(|e| {
            anyhow::anyhow!(
                "deis-telemetry at {} refused the query: {}",
                self.endpoint,
                e.message()
            )
        })?;
        Ok(response.into_inner())
    }

    /// Follow entries as they arrive. Only what is ingested after this call
    /// appears; anything already stored is `query`'s job.
    pub async fn tail(&mut self, request: TailRequest) -> Result<tonic::Streaming<LogEntry>> {
        let response = self
            .client
            .tail(tonic::Request::new(request))
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "deis-telemetry at {} refused to open a tail: {}",
                    self.endpoint,
                    e.message()
                )
            })?;
        Ok(response.into_inner())
    }
}

/// gRPC needs a scheme. A bare `host:port` - which is how the address is
/// written in every component's configuration file - is otherwise rejected
/// with a message about URIs that says nothing about what to fix.
fn normalise_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_and_port_becomes_a_url() {
        assert_eq!(
            normalise_endpoint("127.0.0.1:8092"),
            "http://127.0.0.1:8092"
        );
        assert_eq!(
            normalise_endpoint("http://127.0.0.1:8092/"),
            "http://127.0.0.1:8092"
        );
        assert_eq!(
            normalise_endpoint("https://telemetry.internal"),
            "https://telemetry.internal"
        );
    }

    #[test]
    fn levels_parse_by_name_and_say_so_when_they_do_not() {
        assert_eq!("warn".parse::<Level>().unwrap().0, LogLevel::Warn);
        assert_eq!("WARNING".parse::<Level>().unwrap().0, LogLevel::Warn);
        let error = "loud".parse::<Level>().unwrap_err().to_string();
        assert!(
            error.contains("trace, debug, info, warn, error, fatal"),
            "{error}"
        );
    }

    /// An entry from a component that is not serving one environment - the
    /// node's own startup lines, for instance - must survive the filter, or
    /// `deis logs -e production` hides the reason production is down.
    #[test]
    fn an_entry_without_an_environment_field_is_kept() {
        let mut fields = HashMap::new();
        assert!(entry_matches_environment(&fields, "production"));

        fields.insert(ENVIRONMENT_FIELD.to_string(), "preview".to_string());
        assert!(!entry_matches_environment(&fields, "production"));
        assert!(entry_matches_environment(&fields, "preview"));
    }
}
