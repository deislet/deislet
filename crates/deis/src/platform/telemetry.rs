//! Reading logs and invocation traces back out of deis-telemetry.
//!
//! deis-telemetry knows applications, not tenants: a node ships every
//! environment of one application under the same `app_id` and puts the
//! environment in the entry's `fields` (docs/implementation-plan.zh.md
//! section 1). So `-e/--environment` is a filter this CLI applies to the
//! field, not something the service can be asked to narrow by.
//!
//! Invocation traces are the exception, and it is deliberate: a trace record
//! carries `environment` as its own field, so `QueryTraces` and `TailTraces`
//! both narrow by it on the server. `deis trace -e preview` therefore reads
//! only that environment's records, where `deis logs -e preview` reads the
//! application's pages and drops what does not match.
//!
//! What comes back is also bounded by the credential: every command asks the
//! control plane for a read capability naming the environment it is about
//! (or every environment, with `--all-environments`), and deis-telemetry
//! returns only those tenants' records. See [`TelemetryCredential`].

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};

use crate::platform::Control;
use crate::settings::Settings;

/// Generated client for `deis.telemetry`, compiled from the proto in
/// deis-telemetry by this crate's `build.rs`.
pub mod proto {
    tonic::include_proto!("deis.telemetry");
}

use proto::log_service_client::LogServiceClient;
use proto::trace_service_client::TraceServiceClient;
pub use proto::{
    InvocationKind, InvocationOutcome, LogEntry, LogLevel, QueryRequest, QueryResponse,
    QueryTracesRequest, QueryTracesResponse, TailRequest, TailTracesRequest, TraceRecord,
};

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

/// The capability this CLI presents on every telemetry call.
///
/// Minted by the control plane for exactly what the caller's own scopes
/// reach - one environment, or every environment of an application - the
/// same way `deis kv` gets its storage token. deis-telemetry verifies it
/// against the control plane's published keys and hands back only the
/// tenants it names; it can read and cannot write. There is no secret in
/// this CLI's configuration to derive one from.
///
/// Resolved once per command and handed to `connect`, so every call a
/// command makes carries the same token.
#[derive(Clone)]
pub struct TelemetryCredential {
    /// Pre-formatted `Bearer <token>` value.
    bearer: MetadataValue<Ascii>,
    /// The tenants the token reads, for a refusal that can say so.
    tenants: Vec<String>,
}

impl TelemetryCredential {
    /// Wrap a token the control plane minted for `tenants`.
    pub fn new(token: &str, tenants: Vec<String>) -> Result<Self> {
        let bearer = format!("Bearer {token}")
            .parse::<MetadataValue<Ascii>>()
            .map_err(|_| {
                anyhow::anyhow!(
                    "the control plane returned a telemetry capability that cannot be sent as a \
                     gRPC header"
                )
            })?;
        Ok(Self { bearer, tenants })
    }

    /// Ask the control plane for a read capability for `app`: one
    /// environment, or every environment when `environment` is `None`.
    pub async fn obtain(settings: &Settings, app: &str, environment: Option<&str>) -> Result<Self> {
        let control = Control::new(settings.control_endpoint()?, settings.token())?;
        let minted = control.telemetry_credentials(app, environment).await?;
        Self::new(&minted.token, minted.tenants)
    }

    /// Wrap one message in a request carrying the token.
    fn stamp<T>(&self, message: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        request
            .metadata_mut()
            .insert("authorization", self.bearer.clone());
        request
    }
}

/// Never derived: a derived `Debug` would put the token into any panic message
/// or log line that formats a client.
impl std::fmt::Debug for TelemetryCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryCredential")
            .field("token", &"<present>")
            .field("tenants", &self.tenants)
            .finish()
    }
}

/// A gRPC failure as a sentence, with the hint that is worth attaching.
fn refusal(
    credential: &TelemetryCredential,
    endpoint: &str,
    doing: &str,
    status: &tonic::Status,
) -> anyhow::Error {
    let message = status.message().to_string();
    let reads = credential.tenants.join(", ");
    match status.code() {
        tonic::Code::Unauthenticated => anyhow::anyhow!(
            "deis-telemetry at {endpoint} refused {doing}: {message}\nThis command presented a \
             capability the control plane minted for {reads}. deis-telemetry verifies it against \
             the control plane's published keys (read over its `[machine_identity]` control_endpoint); a refusal means it \
             does not trust this control plane's keys, or the token expired before it was used."
        ),
        tonic::Code::PermissionDenied => anyhow::anyhow!(
            "deis-telemetry at {endpoint} refused {doing}: {message}\nThe capability this \
             command holds reads only {reads}."
        ),
        _ => anyhow::anyhow!("deis-telemetry at {endpoint} refused {doing}: {message}"),
    }
}

/// A client for one deis-telemetry.
pub struct Telemetry {
    client: LogServiceClient<Channel>,
    endpoint: String,
    credential: TelemetryCredential,
}

impl Telemetry {
    /// Dial deis-telemetry, carrying `credential` on every call.
    pub async fn connect(endpoint: &str, credential: TelemetryCredential) -> Result<Self> {
        let url = normalise_endpoint(endpoint);
        let builder = Endpoint::from_shared(url.clone())
            .with_context(|| format!("{endpoint} is not a usable address for deis-telemetry"))?
            .connect_timeout(CONNECT_TIMEOUT);
        let channel = super::service_tls::connect(
            builder,
            &url,
            deis_identity::ServiceKind::Telemetry,
        )
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
            credential,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// One page of stored entries, newest first.
    pub async fn query(&mut self, request: QueryRequest) -> Result<QueryResponse> {
        let mut request = self.credential.stamp(request);
        request.set_timeout(QUERY_TIMEOUT);
        let response = self
            .client
            .query(request)
            .await
            .map_err(|e| refusal(&self.credential, &self.endpoint, "the query", &e))?;
        Ok(response.into_inner())
    }

    /// Follow entries as they arrive. Only what is ingested after this call
    /// appears; anything already stored is `query`'s job.
    pub async fn tail(&mut self, request: TailRequest) -> Result<tonic::Streaming<LogEntry>> {
        let response = self
            .client
            .tail(self.credential.stamp(request))
            .await
            .map_err(|e| refusal(&self.credential, &self.endpoint, "a tail", &e))?;
        Ok(response.into_inner())
    }
}

/// A client for one deis-telemetry's invocation traces.
///
/// Separate from [`Telemetry`] because the two answer different services on
/// the same port: one record per invocation here, one record per log line
/// there. A command wants one or the other, never both.
pub struct Traces {
    client: TraceServiceClient<Channel>,
    endpoint: String,
    credential: TelemetryCredential,
}

impl Traces {
    /// Dial deis-telemetry's trace service, carrying `credential` on every
    /// call - the same kind [`Telemetry::connect`] carries.
    pub async fn connect(endpoint: &str, credential: TelemetryCredential) -> Result<Self> {
        let url = normalise_endpoint(endpoint);
        let builder = Endpoint::from_shared(url.clone())
            .with_context(|| format!("{endpoint} is not a usable address for deis-telemetry"))?
            .connect_timeout(CONNECT_TIMEOUT);
        let channel = super::service_tls::connect(
            builder,
            &url,
            deis_identity::ServiceKind::Telemetry,
        )
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "could not reach deis-telemetry at {url}: {e}\nCheck that it is running and \
                 that the address is right (see `deis config list`)."
            )
        })?;

        let client = TraceServiceClient::new(channel)
            .max_decoding_message_size(deis_wire::conventions::MAX_GRPC_MESSAGE_BYTES);
        Ok(Self {
            client,
            endpoint: url,
            credential,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// One page of stored records, newest invocation first.
    pub async fn query(&mut self, request: QueryTracesRequest) -> Result<QueryTracesResponse> {
        let mut request = self.credential.stamp(request);
        request.set_timeout(QUERY_TIMEOUT);
        let response = self
            .client
            .query_traces(request)
            .await
            .map_err(|e| refusal(&self.credential, &self.endpoint, "the trace query", &e))?;
        Ok(response.into_inner())
    }

    /// Follow records as they arrive. Only invocations that end after this
    /// call appear; anything already stored is `query`'s job.
    pub async fn tail(
        &mut self,
        request: TailTracesRequest,
    ) -> Result<tonic::Streaming<TraceRecord>> {
        let response = self
            .client
            .tail_traces(self.credential.stamp(request))
            .await
            .map_err(|e| refusal(&self.credential, &self.endpoint, "a trace stream", &e))?;
        Ok(response.into_inner())
    }
}

/// Every outcome that is not "the handler returned and we served it".
///
/// The proto's own words for the query this exists for: "which invocations
/// failed in the last hour" is `outcomes` listing everything but OK. The
/// unspecified value is left out on purpose - a record the runtime could not
/// classify is not evidence of a failure, and putting it in would make
/// `--outcome failed` answer with invocations nobody can act on.
pub const FAILED_OUTCOMES: [InvocationOutcome; 6] = [
    InvocationOutcome::ApplicationError,
    InvocationOutcome::ExecutionTimeout,
    InvocationOutcome::RateLimited,
    InvocationOutcome::MemoryLimit,
    InvocationOutcome::InternalError,
    InvocationOutcome::CpuLimit,
];

/// One `--outcome` value.
///
/// `Failed` is not an outcome the platform records; it is the shorthand for
/// the six above, expanded by [`expand_outcomes`] before the request goes
/// out, so the filtering still happens on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeSelector {
    One(InvocationOutcome),
    Failed,
}

impl FromStr for OutcomeSelector {
    type Err = anyhow::Error;

    /// The short name is the one [`outcome_name`] prints, so a value read off
    /// the output can be pasted straight back in as a filter. The longer
    /// spellings are the proto's own names, accepted because that is what
    /// someone who read `telemetry.proto` will type.
    fn from_str(s: &str) -> Result<Self> {
        let outcome = match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "failed" | "failure" | "failures" => return Ok(Self::Failed),
            "ok" => InvocationOutcome::Ok,
            "app-error" | "error" | "application-error" => InvocationOutcome::ApplicationError,
            "timeout" | "execution-timeout" => InvocationOutcome::ExecutionTimeout,
            "rate-limited" => InvocationOutcome::RateLimited,
            "memory-limit" | "memory" => InvocationOutcome::MemoryLimit,
            "cpu-limit" | "cpu" => InvocationOutcome::CpuLimit,
            "internal-error" | "internal" => InvocationOutcome::InternalError,
            other => bail!(
                "unknown outcome {other:?}; use one of ok, app-error, timeout, rate-limited, \
                 memory-limit, cpu-limit, internal-error, or failed for every one but ok"
            ),
        };
        Ok(Self::One(outcome))
    }
}

/// Turn what was asked for into the list `QueryTracesRequest.outcomes` and
/// `TailTracesRequest.outcomes` carry. An empty list matches everything,
/// which is what no `--outcome` at all means.
pub fn expand_outcomes(selectors: &[OutcomeSelector]) -> Vec<i32> {
    let mut wanted: Vec<i32> = Vec::new();
    for selector in selectors {
        match selector {
            OutcomeSelector::One(outcome) => wanted.push(*outcome as i32),
            OutcomeSelector::Failed => wanted.extend(FAILED_OUTCOMES.map(|o| o as i32)),
        }
    }
    wanted.sort_unstable();
    wanted.dedup();
    wanted
}

/// One `--kind` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kind(pub InvocationKind);

impl FromStr for Kind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let kind = match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "http" | "http-request" | "request" => InvocationKind::HttpRequest,
            "queue" | "queue-batch" => InvocationKind::QueueBatch,
            "cron" | "cron-tick" => InvocationKind::CronTick,
            "websocket" | "websocket-message" | "ws" => InvocationKind::WebsocketMessage,
            other => {
                bail!("unknown invocation kind {other:?}; use one of http, queue, cron, websocket")
            }
        };
        Ok(Self(kind))
    }
}

/// The name to print, and to accept back, for one record's outcome.
///
/// Deliberately one vocabulary: what a row shows is what `--outcome` takes
/// and what `--json` carries, so nobody has to translate between three
/// spellings of the same fact.
pub fn outcome_name(outcome: i32) -> &'static str {
    match InvocationOutcome::try_from(outcome).unwrap_or(InvocationOutcome::Unspecified) {
        InvocationOutcome::Ok => "ok",
        InvocationOutcome::ApplicationError => "app-error",
        InvocationOutcome::ExecutionTimeout => "timeout",
        InvocationOutcome::RateLimited => "rate-limited",
        InvocationOutcome::MemoryLimit => "memory-limit",
        InvocationOutcome::CpuLimit => "cpu-limit",
        InvocationOutcome::InternalError => "internal-error",
        // A node that could not classify an invocation sends this rather
        // than guessing; printing "unknown" keeps that distinction visible
        // instead of quietly filing it under ok.
        InvocationOutcome::Unspecified => "unknown",
    }
}

/// The name to print, and to accept back, for one record's kind.
pub fn kind_name(kind: i32) -> &'static str {
    match InvocationKind::try_from(kind).unwrap_or(InvocationKind::Unspecified) {
        InvocationKind::HttpRequest => "http",
        InvocationKind::QueueBatch => "queue",
        InvocationKind::CronTick => "cron",
        InvocationKind::WebsocketMessage => "websocket",
        InvocationKind::Unspecified => "unknown",
    }
}

/// True when the platform, not the application, ended the invocation - the
/// same judgement `InvocationTrace::is_failure` makes on the node.
pub fn is_failure(outcome: i32) -> bool {
    FAILED_OUTCOMES.iter().any(|o| *o as i32 == outcome)
}

/// gRPC needs a scheme. A bare `host:port` - which is how the address is
/// written in every component's configuration file - is otherwise rejected
/// with a message about URIs that says nothing about what to fix.
fn normalise_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use deis_capability::{
        AUD_TELEMETRY, CapabilityClaims, CapabilityResource, KeyRing, TELEMETRY_READ, encode,
    };

    /// The control plane's signing key, as far as these tests are concerned.
    fn control_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[0x33; 32])
    }

    /// A read capability for `tenants`, signed by `key`.
    /// A read capability for the environments `labels` name, minted the way
    /// the control plane mints it: for their canonical tenants.
    fn minted(key: &ed25519_dalek::SigningKey, labels: &[&str]) -> TelemetryCredential {
        let tenants: Vec<String> = labels
            .iter()
            .map(|label| deis_wire::CanonicalTenantId::derived_from_label(label).to_string())
            .collect();
        let claims = CapabilityClaims::new(
            AUD_TELEMETRY,
            "spiffe://deis.test/deislet/control/main",
            "spiffe://deislet/human/dev",
            1,
            std::time::Duration::from_secs(900),
            tenants
                .iter()
                .map(|tenant| CapabilityResource {
                    tenant: tenant.to_string(),
                    binding: TELEMETRY_READ.to_string(),
                    generation: None,
                })
                .collect(),
        );
        TelemetryCredential::new(&encode(&claims, key), tenants)
            .expect("a v1 capability is a legal header value")
    }

    /// A credential must not be printable, in a panic or anywhere else.
    #[test]
    fn the_token_never_reaches_a_debug_line() {
        let credential = minted(&control_key(), &["demo.production"]);
        let token = credential.bearer.to_str().unwrap().to_string();
        let rendered = format!("{credential:?}");
        assert!(rendered.contains("<present>"), "{rendered}");
        let tenant =
            deis_wire::CanonicalTenantId::derived_from_label("demo.production").to_string();
        assert!(rendered.contains(&tenant), "{rendered}");
        assert!(!rendered.contains(&token["Bearer ".len()..]), "{rendered}");
    }

    /// Start a real collector trusting the control key. Both listeners are on
    /// loopback port 0, so this never collides with a running install.
    async fn collector() -> (deis_telemetry::server::RunningServer, tempfile::TempDir) {
        let data = tempfile::tempdir().expect("a directory for the database");
        let config = deis_telemetry::config::Config {
            listen: "127.0.0.1:0".to_string(),
            metrics_listen: "127.0.0.1:0".to_string(),
            data_dir: data.path().to_path_buf(),
            machine_identity: Some(collector_identity()),
            ..deis_telemetry::config::Config::default()
        };
        let running = deis_telemetry::server::start(
            &config,
            KeyRing::from_keys([control_key().verifying_key()]),
        )
        .await
        .expect("deis-telemetry starts");
        (running, data)
    }

    /// The collector's `[machine_identity]`, issued by the authority this
    /// CLI is configured to trust.
    fn collector_identity() -> deis_capability::ServiceIdentityConfig {
        let fleet = crate::platform::service_tls::test_fleet();
        let id = deis_identity::names::telemetry(&fleet.trust_domain, "test").unwrap();
        let files = fleet.files_for("telemetry", &id);
        deis_capability::ServiceIdentityConfig {
            certificate: files.certificate,
            private_key: files.private_key,
            trust_root: files.trust_root,
            // A stand-in control plane serving revocation lists signed with
            // the control key; the collector refuses everything until it has
            // adopted one.
            control_endpoint: {
                static CONTROL: std::sync::OnceLock<deis_capability::testing::FakeControl> =
                    std::sync::OnceLock::new();
                CONTROL
                    .get_or_init(|| deis_capability::testing::FakeControl::start(control_key()))
                    .url()
                    .to_string()
            },
            identity_reload_seconds: 60,
        }
    }

    /// The read half of observability, against the real collector: all four
    /// call sites present the capability, and each refusal says what the
    /// capability was for.
    #[tokio::test]
    async fn every_telemetry_read_presents_the_capability_and_says_so_when_refused() {
        let (server, _data) = collector().await;
        let endpoint = format!("https://{}", server.grpc_addr);
        let reader = minted(&control_key(), &["demo.production"]);

        let mut logs = Telemetry::connect(&endpoint, reader.clone())
            .await
            .expect("dialling works");
        logs.query(QueryRequest {
            app_id: "demo".to_string(),
            limit: 5,
            ..Default::default()
        })
        .await
        .expect("`deis logs` reads the application its capability names");
        logs.tail(TailRequest {
            app_id: "demo".to_string(),
            ..Default::default()
        })
        .await
        .expect("and `deis tail` opens a stream");

        let mut traces = Traces::connect(&endpoint, reader.clone())
            .await
            .expect("dialling works");
        traces
            .query(QueryTracesRequest {
                app_id: "demo".to_string(),
                limit: 5,
                ..Default::default()
            })
            .await
            .expect("`deis trace` reads the other service on the same credential");
        traces
            .tail(TailTracesRequest {
                app_id: "demo".to_string(),
                ..Default::default()
            })
            .await
            .expect("and follows it");

        // Another application: the capability names tenants by id, so the
        // name is not refused, and nothing of bank comes back.
        let page = logs
            .query(QueryRequest {
                app_id: "bank".to_string(),
                ..Default::default()
            })
            .await
            .expect("a named read is answered");
        assert!(page.entries.is_empty(), "a capability for demo read bank");
        // A read naming no application is refused, and the message says
        // what the capability reads.
        let error = logs
            .query(QueryRequest::default())
            .await
            .expect_err("a narrow capability reads named applications only")
            .to_string();
        assert!(error.contains("reads only"), "{error}");

        // A capability some other key signed: refused as unauthenticated, and
        // the message points at the collector's trust in the control plane.
        let forged = minted(
            &ed25519_dalek::SigningKey::from_bytes(&[1; 32]),
            &["demo.production"],
        );
        let mut logs = Telemetry::connect(&endpoint, forged)
            .await
            .expect("dialling works");
        let error = logs
            .query(QueryRequest {
                app_id: "demo".to_string(),
                ..Default::default()
            })
            .await
            .expect_err("a capability the control plane did not sign is refused")
            .to_string();
        assert!(error.contains("control_endpoint"), "{error}");

        server.shutdown().await.expect("the collector stops");
    }

    #[test]
    fn a_bare_host_and_port_becomes_a_url() {
        assert_eq!(
            normalise_endpoint("127.0.0.1:8092"),
            "https://127.0.0.1:8092"
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

    #[test]
    fn outcomes_parse_by_the_name_they_are_printed_with() {
        assert_eq!(
            "app-error".parse::<OutcomeSelector>().unwrap(),
            OutcomeSelector::One(InvocationOutcome::ApplicationError)
        );
        // The proto's own spelling, for someone who read telemetry.proto.
        assert_eq!(
            "APPLICATION_ERROR".parse::<OutcomeSelector>().unwrap(),
            OutcomeSelector::One(InvocationOutcome::ApplicationError)
        );
        assert_eq!(
            "cpu".parse::<OutcomeSelector>().unwrap(),
            OutcomeSelector::One(InvocationOutcome::CpuLimit)
        );
        let error = "broken".parse::<OutcomeSelector>().unwrap_err().to_string();
        assert!(error.contains("rate-limited"), "{error}");
    }

    /// `--outcome failed` has to reach deis-telemetry as the list of failing
    /// outcomes, or the filtering would silently happen nowhere: the service
    /// treats an empty list as "every outcome".
    #[test]
    fn failed_expands_to_every_outcome_but_ok() {
        let wanted = expand_outcomes(&["failed".parse().unwrap()]);
        assert_eq!(wanted.len(), FAILED_OUTCOMES.len());
        assert!(!wanted.contains(&(InvocationOutcome::Ok as i32)));
        assert!(!wanted.contains(&(InvocationOutcome::Unspecified as i32)));
        assert!(wanted.contains(&(InvocationOutcome::MemoryLimit as i32)));
        assert!(wanted.iter().all(|o| is_failure(*o)));
    }

    /// Asking for the same outcome twice - directly and through `failed` -
    /// must not send it twice; deis-telemetry would then match it twice.
    #[test]
    fn overlapping_outcome_filters_collapse() {
        let wanted = expand_outcomes(&["failed".parse().unwrap(), "timeout".parse().unwrap()]);
        assert_eq!(wanted.len(), FAILED_OUTCOMES.len());
    }

    #[test]
    fn no_outcome_filter_matches_everything() {
        assert!(expand_outcomes(&[]).is_empty());
    }

    #[test]
    fn kinds_parse_by_the_name_they_are_printed_with() {
        assert_eq!(
            "http".parse::<Kind>().unwrap().0,
            InvocationKind::HttpRequest
        );
        assert_eq!("cron".parse::<Kind>().unwrap().0, InvocationKind::CronTick);
        assert_eq!(kind_name(InvocationKind::QueueBatch as i32), "queue");
        assert!("mail".parse::<Kind>().is_err());
    }

    /// Every name this CLI prints has to parse back into the outcome it
    /// came from, or a filter copied off a row would be refused.
    #[test]
    fn every_printed_outcome_name_is_accepted_back() {
        for outcome in [
            InvocationOutcome::Ok,
            InvocationOutcome::ApplicationError,
            InvocationOutcome::ExecutionTimeout,
            InvocationOutcome::RateLimited,
            InvocationOutcome::MemoryLimit,
            InvocationOutcome::CpuLimit,
            InvocationOutcome::InternalError,
        ] {
            let name = outcome_name(outcome as i32);
            assert_eq!(
                name.parse::<OutcomeSelector>().unwrap(),
                OutcomeSelector::One(outcome),
                "{name} did not parse back"
            );
        }
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
