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

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use deis_wire::conventions::{STORAGE_TOKEN_HEADER, storage_token};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};

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

/// What the capability token for deis-telemetry is derived over.
///
/// The subject is the only thing separating this token from a storage token
/// minted from the same secret, so it has to be the string the collector
/// itself uses: `deis_telemetry::auth::TELEMETRY_CHANNEL`. Copied rather than
/// imported because this CLI does not depend on that crate - doing so would
/// link the collector's server, its database and its HTTP stack into a
/// developer tool - and `the_channel_subject_is_the_collectors_own` at the
/// bottom of this file, which does depend on it as a test-only dependency, is
/// what stops the two drifting apart. `deis_runtime::telemetry` holds a third
/// copy for the same reason and is pinned the same way.
const TELEMETRY_CHANNEL: &str = "deis-telemetry";

/// The token this CLI presents on every telemetry call, or nothing when no
/// secret is configured.
///
/// Derived from a configured secret rather than fetched from the control
/// plane, which is where `deis kv` and `deis queue` get theirs: the control
/// plane's storage-token route mints a *tenant* credential from deis-store's
/// secret, and this token names the channel rather than a tenant, so there is
/// no route that could hand it out. The one other holder of this credential -
/// a node, in `deis_runtime::telemetry::TelemetryCredential` - reads the same
/// string from its own configuration and derives it the same way.
///
/// Until 2026-09-02 there was no credential here at all. `deis logs`, `deis
/// tail` and `deis trace` built a bare channel, so against any deis-telemetry
/// with an `auth_secret` - which it must have as soon as either of its
/// listeners is not on loopback - every one of them answered "refused the
/// query: a valid x-deis-storage-token is required", and there was no setting
/// that could have fixed it. kv, db, blob and queue all obtained and presented
/// a token; this was the one consumer that never got one.
#[derive(Clone, Default)]
pub struct TelemetryCredential {
    /// Parsed once, when the client is built, so a secret that cannot be sent
    /// as gRPC metadata is one complaint rather than one per call.
    token: Option<MetadataValue<Ascii>>,
    /// Where the secret was found, for a refusal that can say which file or
    /// variable to edit. Never the value.
    source: Option<String>,
}

impl TelemetryCredential {
    /// Find the secret for `endpoint` and derive the token.
    ///
    /// Resolved from the endpoint inside `connect` rather than passed in by
    /// each command: there are four `connect` call sites across `deis logs`,
    /// `deis tail` and `deis trace`, and a credential handed in as an argument
    /// is a credential one of the four forgets. That is the shape of the
    /// defect this replaces.
    fn resolve(endpoint: &str) -> Result<Self> {
        let Some(setting) = crate::settings::telemetry_auth_secret(endpoint)? else {
            return Ok(Self::default());
        };
        // A blank secret is already filtered out by the resolver, so anything
        // arriving here is a real value.
        let derived = storage_token(&setting.value, TELEMETRY_CHANNEL);
        let token = derived.parse::<MetadataValue<Ascii>>().map_err(|_| {
            // The derivation is base64 of an HMAC, so no secret of any shape
            // produces this. Said out loud rather than silently dropped,
            // because shipping without the token means every read is refused.
            anyhow::anyhow!(
                "the deis-telemetry capability token derived from the secret in {} cannot be sent \
                 as a gRPC header, so this command has no way to authenticate",
                setting.source
            )
        })?;
        Ok(Self {
            token: Some(token),
            source: Some(setting.source.to_string()),
        })
    }

    /// Wrap one message in a request carrying the token.
    ///
    /// Stamped here rather than at each call for the reason
    /// `platform::queue::Identity::stamp` gives: a call that forgot it would
    /// be refused, and nothing about the failure would say which call it was.
    fn stamp<T>(&self, message: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        if let Some(token) = &self.token {
            request
                .metadata_mut()
                .insert(STORAGE_TOKEN_HEADER, token.clone());
        }
        request
    }
}

/// Never derived: a derived `Debug` would put the token into any panic message
/// or log line that formats a client, which is exactly where a credential must
/// never appear. The same reasoning as `platform::storage::Identity`.
impl std::fmt::Debug for TelemetryCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryCredential")
            .field(
                "token",
                &if self.token.is_some() {
                    "<present>"
                } else {
                    "<absent>"
                },
            )
            .field("source", &self.source.as_deref().unwrap_or("<none>"))
            .finish()
    }
}

/// A gRPC failure as a sentence, with the hint that is worth attaching.
///
/// `Unauthenticated` is the failure an operator actually hits, and the cause is
/// never in the status text - deis-telemetry does not say which secret it
/// checked against, because saying so would leak how the token is derived. So
/// this side says what it presented and where the value came from, which is
/// the half the operator can act on.
fn refusal(
    credential: &TelemetryCredential,
    endpoint: &str,
    doing: &str,
    status: &tonic::Status,
) -> anyhow::Error {
    let message = status.message().to_string();
    if status.code() != tonic::Code::Unauthenticated {
        return anyhow::anyhow!("deis-telemetry at {endpoint} refused {doing}: {message}");
    }
    match &credential.source {
        Some(source) => anyhow::anyhow!(
            "deis-telemetry at {endpoint} refused {doing}: {message}\nThis command presented the \
             token derived from the secret in {source}. deis-telemetry derives what it expects \
             from its own `auth_secret`; when the two are not the same string every node's log, \
             trace and metric push is being refused as well."
        ),
        None => anyhow::anyhow!(
            "deis-telemetry at {endpoint} refused {doing}: {message}\nThis command presented no \
             credential, because none is configured. Set the same string deis-telemetry's \
             `auth_secret` is set to:\n\n  {}=<secret>\n\nor put it in {} under\n\n  \
             [telemetry.\"{endpoint}\"]\n  auth_secret = \"<secret>\"",
            crate::settings::ENV_TELEMETRY_AUTH_SECRET,
            crate::settings::credentials_path().display()
        ),
    }
}

/// A client for one deis-telemetry.
pub struct Telemetry {
    client: LogServiceClient<Channel>,
    endpoint: String,
    credential: TelemetryCredential,
}

impl Telemetry {
    /// Dial deis-telemetry, carrying whatever credential is configured for
    /// that address.
    ///
    /// The credential is resolved here rather than taken as an argument: see
    /// [`TelemetryCredential::resolve`]. The signature is unchanged for the
    /// same reason - every existing caller gets the token without being edited,
    /// and a caller added later cannot forget it.
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let url = normalise_endpoint(endpoint);
        let credential = TelemetryCredential::resolve(&url)?;
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
    /// Dial deis-telemetry's trace service, carrying the same credential
    /// [`Telemetry::connect`] carries - one collector, one `auth_secret`, one
    /// token. Two derivations would be two places for the subject to be
    /// spelled wrong.
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let url = normalise_endpoint(endpoint);
        let credential = TelemetryCredential::resolve(&url)?;
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
        format!("http://{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::settings::{
        Credentials, ENV_CONFIG_HOME, ENV_TELEMETRY_AUTH_SECRET, TelemetryEntry, clear_env,
        env_lock, save_credentials, set_env,
    };

    /// The subject has to be the collector's own, or the two derive different
    /// tokens from the same secret and every read is refused.
    ///
    /// This is the whole reason `deis-telemetry` is a dev-dependency of this
    /// crate: the constant is copied rather than imported, and a copy with
    /// nothing pinning it is a copy that drifts.
    #[test]
    fn the_channel_subject_is_the_collectors_own() {
        assert_eq!(TELEMETRY_CHANNEL, deis_telemetry::auth::TELEMETRY_CHANNEL);
    }

    /// A credential must not be printable, in a panic or anywhere else.
    #[test]
    fn the_token_never_reaches_a_debug_line() {
        let secret = "shared-secret";
        let credential = TelemetryCredential {
            token: Some(
                storage_token(secret, TELEMETRY_CHANNEL)
                    .parse()
                    .expect("an HMAC in base64 is a legal header value"),
            ),
            source: Some("credentials.toml".to_string()),
        };
        let rendered = format!("{credential:?}");
        assert!(rendered.contains("<present>"), "{rendered}");
        assert!(
            !rendered.contains(&storage_token(secret, TELEMETRY_CHANNEL)),
            "the derived token is the whole authority to read every application's telemetry: \
             {rendered}"
        );
        assert!(!rendered.contains(secret), "{rendered}");
    }

    /// Start a real collector that requires a token. Both listeners are on
    /// loopback port 0, so this never collides with a running install.
    async fn collector(secret: &str) -> (deis_telemetry::server::RunningServer, tempfile::TempDir) {
        let data = tempfile::tempdir().expect("a directory for the database");
        let config = deis_telemetry::config::Config {
            listen: "127.0.0.1:0".to_string(),
            metrics_listen: "127.0.0.1:0".to_string(),
            data_dir: data.path().to_path_buf(),
            auth_secret: Some(secret.to_string()),
            ..deis_telemetry::config::Config::default()
        };
        let running = deis_telemetry::server::start(&config)
            .await
            .expect("deis-telemetry starts");
        (running, data)
    }

    /// Point this process's credentials file at a fresh directory holding one
    /// secret for `endpoint`.
    fn configure(home: &std::path::Path, endpoint: &str, secret: Option<&str>) {
        set_env(ENV_CONFIG_HOME, &home.to_string_lossy());
        let mut credentials = Credentials::default();
        if let Some(secret) = secret {
            credentials.telemetry.insert(
                endpoint.to_string(),
                TelemetryEntry {
                    auth_secret: secret.to_string(),
                },
            );
        }
        save_credentials(&credentials).expect("write the credentials file");
    }

    /// The read half of observability, against a collector that authenticates.
    ///
    /// Until 2026-09-02 `Telemetry::connect` and `Traces::connect` built a bare
    /// `Channel` and stamped nothing, so all four of these calls answered
    /// "a valid x-deis-storage-token is required" and no setting could have
    /// fixed it - while kv, db, blob and queue all obtained and presented a
    /// token. Four calls in one test on purpose: they are four separate call
    /// sites, and giving one of them a credential is exactly the mistake being
    /// repaired.
    ///
    /// One test rather than four because it moves the process environment, and
    /// `env_lock` is held from the first line to the last.
    // The environment lock is deliberately held across every await here: what
    // is under test is behaviour that depends on the process environment, so
    // releasing it between calls would let a sibling test change the variables
    // half way through. It cannot deadlock - this test's runtime is
    // single-threaded and nothing on it takes the lock, and the synchronous
    // tests that want it simply block on another thread, which is the point.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn every_telemetry_read_presents_the_token_and_says_so_when_it_cannot() {
        const SECRET: &str = "the same string on both sides";
        let _guard = env_lock();
        clear_env();

        let (server, _data) = collector(SECRET).await;
        let endpoint = format!("http://{}", server.grpc_addr);

        // 1. Configured with the collector's own secret: every read works.
        let home = tempfile::tempdir().expect("a config home");
        configure(home.path(), &endpoint, Some(SECRET));

        let mut logs = Telemetry::connect(&endpoint).await.expect("dialling works");
        logs.query(QueryRequest {
            limit: 5,
            ..Default::default()
        })
        .await
        .expect("`deis logs` has to be able to read a collector it holds the secret for");
        logs.tail(TailRequest::default())
            .await
            .expect("and `deis tail` has to open a stream");

        let mut traces = Traces::connect(&endpoint).await.expect("dialling works");
        traces
            .query(QueryTracesRequest {
                limit: 5,
                ..Default::default()
            })
            .await
            .expect("`deis trace` reads the other service on the same credential");
        traces
            .tail(TailTracesRequest::default())
            .await
            .expect("and follows it");

        // 2. Nothing configured: the refusal has to say what to set, because
        //    the collector's own message never can - it will not name the
        //    secret it checked against.
        let empty = tempfile::tempdir().expect("a second config home");
        configure(empty.path(), &endpoint, None);

        let mut logs = Telemetry::connect(&endpoint).await.expect("dialling works");
        let error = logs
            .query(QueryRequest::default())
            .await
            .expect_err("a collector with an auth_secret refuses an unauthenticated read")
            .to_string();
        assert!(
            error.contains(ENV_TELEMETRY_AUTH_SECRET),
            "the message has to name the variable to set: {error}"
        );
        assert!(
            error.contains("auth_secret"),
            "and the file entry that is the other way: {error}"
        );
        assert!(
            error.contains("no credential"),
            "and that this end presented nothing, which is the half the collector cannot say: \
             {error}"
        );

        // 3. Configured with the wrong secret: a different situation and a
        //    different sentence - there is nothing to *add*, something has to
        //    be corrected, and the message says where the value came from.
        let wrong = tempfile::tempdir().expect("a third config home");
        configure(wrong.path(), &endpoint, Some("not the collector's secret"));

        let mut logs = Telemetry::connect(&endpoint).await.expect("dialling works");
        let error = logs
            .query(QueryRequest::default())
            .await
            .expect_err("a token derived from the wrong secret is refused")
            .to_string();
        assert!(
            error.contains("credentials.toml"),
            "the message has to say where the secret it used came from: {error}"
        );
        assert!(
            !error.contains("not the collector's secret"),
            "and must never print the secret itself: {error}"
        );

        // 4. The environment variable is the other way to say it, and it wins.
        set_env(ENV_TELEMETRY_AUTH_SECRET, SECRET);
        let mut logs = Telemetry::connect(&endpoint).await.expect("dialling works");
        logs.query(QueryRequest {
            limit: 5,
            ..Default::default()
        })
        .await
        .expect("$DEIS_TELEMETRY_AUTH_SECRET beats the wrong value in the file");

        clear_env();
        server.shutdown().await.expect("the collector stops");
    }

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
