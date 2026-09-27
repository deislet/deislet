//! Fetching the routing table from the control plane.
//!
//! # One transport
//!
//! The table is fetched as `deis.machine.MachineChannel/FetchRoutingTable` on
//! the control plane's machine channel, authenticated by a certificate that
//! names this proxy and nothing else, and answered by a control plane that has
//! proved its own name back.
//!
//! There used to be a second way: an HTTP GET on the control plane's human
//! listener, carrying the fleet-wide `node_preshared_keys` value that every
//! process in the fleet held a copy of. Both the value and the route are gone.
//! What is left of that arm here is one error: a proxy configured without a
//! `[machine_identity]` table has nothing to prove and no route to ask, and
//! says so rather than dialling something that would answer 404 for the life
//! of the process. `SystemConfig::validate` refuses such a configuration at
//! startup, so in practice that error is only reachable by assembling a client
//! by hand.
//!
//! The document did not change when the transport did. It is the same JSON,
//! parsed and validated by the same [`super::wire::RoutingTableDocument`] code
//! under the same all-or-nothing rule. The two values that used to be request
//! headers - which proxy is asking, and which version of the table it is
//! already running - are fields on the RPC and carry exactly what the headers
//! carried.
//!
//! # What this costs the process
//!
//! The machine channel is tonic over hyper, and that is not a second stack
//! this module introduces: deis-wire's control client is tonic, so this binary
//! has linked tonic and hyper since the day it grew a control channel -
//! `cargo tree -p deis-proxy -e normal -i tonic` shows the edge, and `-i hyper`
//! shows that the only hyper in the tree comes in behind it, not behind
//! pingora. What is new here is that the catalog poller uses them too, not that
//! they are present.
//!
//! One connection is opened per poll and dropped when the table has been read.
//! Nothing is pooled on purpose: a poll happens every few seconds, and a pooled
//! connection to the control plane would be one more socket to carry across a
//! zero-downtime upgrade for no measurable saving. It has a second effect worth
//! having - the handshake is made again on every poll, so a certificate renewed
//! on disk is presented by the next poll without anything having to be told it
//! changed.
//!
//! The whole exchange is under one timeout. A control plane that accepts the
//! TCP connection and then says nothing is the failure mode that would
//! otherwise park the poll loop for ever, and a parked poll loop is a data
//! plane that stops noticing route changes while looking perfectly healthy.
//! [`CatalogClient::fetch`] is the bound, and it wraps connect, handshake,
//! request and body alike. It hands tonic the same duration again as its
//! connect timeout and its per-request timeout, which changes no guarantee but
//! reports the ordinary failures as what they are instead of delivering all of
//! them as one stopwatch running out.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use deis_identity::{Dialler, MachineIdentity, Protocol};
use deis_wire::ControlConfig;
use deis_wire::machine::FetchRoutingTableRequest;
use deis_wire::machine::machine_channel_client::MachineChannelClient;
use tokio::time::timeout;
use tonic::transport::{Channel, Endpoint};
use tracing::debug;

use super::wire::{DocumentError, RoutingTableDocument};
use crate::config::system::MachineIdentityConfig;

/// The gRPC method, spelled out for log lines only.
///
/// tonic builds the real path from the generated client; this is here so a
/// poller log line can say where it is asking in the same field whichever
/// transport is in use, rather than saying `path` on one and nothing on the
/// other.
const FETCH_ROUTING_TABLE_METHOD: &str = "/deis.machine.MachineChannel/FetchRoutingTable";

/// Everything a fetch can fail with.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("control plane address {addr:?} is not usable: {reason}")]
    Address { addr: String, reason: String },
    #[error("{addr} did not resolve: {source}")]
    Resolve {
        addr: String,
        source: std::io::Error,
    },
    #[error("no answer from the control plane within {0:?}")]
    Timeout(Duration),
    #[error("{0}")]
    Transport(String),
    #[error(
        "the control plane answered {status} on {path}; it holds the routing table, so this is a control plane fault, not a routing one{detail}"
    )]
    Status {
        status: u16,
        path: &'static str,
        detail: String,
    },
    #[error(
        "the machine channel refused this proxy's routing-table call: {message}. A control plane answers only for the proxy the connection proved, so the certificate named by machine_identity.certificate and control.associated_id ({proxy_id:?}) have to be the same proxy"
    )]
    Denied { proxy_id: String, message: String },
    #[error("the machine channel did not answer the routing-table call ({code}): {message}")]
    Rpc { code: String, message: String },
    #[error(
        "the routing table is larger than the {limit} byte ceiling; raise routing.max_table_bytes if the platform really has this many routes"
    )]
    TooLarge { limit: usize },
    #[error("the routing table was cut short: {got} bytes arrived of the {declared} announced")]
    Truncated { got: usize, declared: usize },
    #[error("the routing table is not the JSON this build understands: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("the routing table is not usable: {0}")]
    Invalid(#[from] DocumentError),
}

/// How a connection to the machine channel is opened.
///
/// A trait rather than the [`Dialler`] outright, because a mutual TLS
/// handshake needs a certificate authority, two issued certificates and names
/// this fleet would recognise - none of which a test of *this* module's
/// request and decode has any business standing up. Production has exactly
/// one implementation, [`DialTheControlPlane`] below; the tests here
/// substitute a plain connection to an in-process server, so what they
/// exercise is this module rather than deis-identity, whose handshake has an
/// end-to-end test of its own against the real listener in deis-control.
#[async_trait]
trait MachineConnect: Send + Sync + 'static {
    async fn connect(&self) -> Result<Channel, FetchError>;
}

/// The production connector: mutual TLS to one named control plane.
struct DialTheControlPlane {
    /// `https://host:port`. The authority in it is where the socket is
    /// opened; the host name in it is checked against nothing. What decides
    /// whether the far end is the control plane is the SPIFFE name the
    /// dialler requires, so dialling this by bare IP address is normal here
    /// rather than a shortcut. The scheme is only what tonic wants to see in
    /// a URI - the handshake is the dialler's either way. See
    /// `deis_identity::dial`.
    uri: String,
    dialler: Dialler,
    timeout: Duration,
}

#[async_trait]
impl MachineConnect for DialTheControlPlane {
    async fn connect(&self) -> Result<Channel, FetchError> {
        Endpoint::from_shared(self.uri.clone())
            .map_err(|error| FetchError::Address {
                addr: self.uri.clone(),
                reason: error.to_string(),
            })?
            .connect_timeout(self.timeout)
            .timeout(self.timeout)
            // Cloned per poll: `Dialler` is a handle onto the identity, not a
            // captured configuration, so the clone resolves the certificate
            // in force at the moment it dials.
            .connect_with_connector(self.dialler.clone())
            .await
            .map_err(|error| {
                FetchError::Transport(format!(
                    "reaching the machine channel at {}: {error}",
                    self.uri
                ))
            })
    }
}

/// One of the two ways the same document is fetched.
enum Transport {
    /// The machine channel, with this proxy's own certificate.
    Machine {
        /// `host:port`, for logs. What the connection is actually made to is
        /// the connector's business.
        authority: String,
        connect: Box<dyn MachineConnect>,
    },
}

/// Reads the routing table. One of these lives for the life of the process.
pub struct CatalogClient {
    transport: Transport,
    request_timeout: Duration,
    max_table_bytes: usize,
    proxy_id: Option<String>,
    observed_routing_table: RwLock<Option<String>>,
}

impl CatalogClient {
    /// Fetch over the machine channel, proving `identity` and requiring
    /// `control` to be proved back.
    ///
    /// `endpoint` is `https://host:port` as written in
    /// `machine_identity.control_endpoint`; the configuration has already
    /// refused anything that is not that shape, port included.
    pub fn over_machine_channel(
        endpoint: String,
        identity: Arc<MachineIdentity>,
        control: deis_identity::SpiffeId,
        request_timeout: Duration,
        max_table_bytes: usize,
    ) -> Self {
        // The scheme comes off for the log field only, so that one `control =`
        // in the poller's lines reads the same whichever transport wrote it.
        // Every other use of the endpoint keeps it: tonic wants a URI.
        let authority = endpoint
            .strip_prefix("https://")
            .unwrap_or(&endpoint)
            .to_string();
        Self::with_transport(
            Transport::Machine {
                authority,
                connect: Box::new(DialTheControlPlane {
                    uri: endpoint,
                    // HTTP/2 because that is what gRPC is. Asking for it by
                    // ALPN rather than hoping for it means a control plane
                    // that would only speak HTTP/1.1 fails at the handshake,
                    // where the reason is legible, instead of at a request
                    // the far end cannot frame.
                    dialler: Dialler::new(identity, control, &[Protocol::Http2]),
                    timeout: request_timeout,
                }),
            },
            request_timeout,
            max_table_bytes,
        )
    }

    fn with_transport(
        transport: Transport,
        request_timeout: Duration,
        max_table_bytes: usize,
    ) -> Self {
        Self {
            transport,
            request_timeout,
            max_table_bytes,
            proxy_id: None,
            observed_routing_table: RwLock::new(None),
        }
    }

    pub fn with_proxy_id(mut self, proxy_id: String) -> Self {
        self.proxy_id = Some(proxy_id);
        self
    }

    /// Called only after ConfigManager has published the complete validated
    /// table. The next poll carries this as observed state, deliberately one
    /// exchange later than receipt.
    pub fn observe_routing_table(&self, version: String) {
        *self
            .observed_routing_table
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(version);
    }

    /// `host:port` of whatever this client asks, for log lines.
    pub fn authority(&self) -> String {
        match &self.transport {
            Transport::Machine { authority, .. } => authority.clone(),
        }
    }

    /// Which channel this client fetches on, in the words a log line has room
    /// for. Also what a test asserts on to check the choice was made the way
    /// the configuration asked for.
    pub fn transport(&self) -> &'static str {
        match &self.transport {
            Transport::Machine { .. } => "machine channel",
        }
    }

    /// What it asks for there: an HTTP route on one transport, a gRPC method
    /// on the other.
    pub fn path(&self) -> &'static str {
        match &self.transport {
            Transport::Machine { .. } => FETCH_ROUTING_TABLE_METHOD,
        }
    }

    /// The version of the table this proxy is running, or an empty string if
    /// it has published none yet.
    ///
    /// Empty is the "nothing applied yet" signal on both transports: the HTTP
    /// path leaves the header off entirely, and `machine.proto` says a caller
    /// that has applied nothing leaves the field empty and simply gets the
    /// table.
    fn observed(&self) -> Option<String> {
        self.observed_routing_table
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// One complete fetch, or an error. Nothing partial is ever returned:
    /// the document is parsed and validated whole before it is handed back.
    pub async fn fetch(&self) -> Result<RoutingTableDocument, FetchError> {
        let exchange = async {
            let Transport::Machine { connect, .. } = &self.transport;
            self.fetch_over_machine_channel(connect.as_ref()).await
        };
        match timeout(self.request_timeout, exchange).await {
            Ok(result) => result,
            Err(_) => Err(FetchError::Timeout(self.request_timeout)),
        }
    }

    /// Parse, validate and log one fetched document, whichever transport
    /// carried it.
    ///
    /// Shared on purpose rather than duplicated per transport: the document
    /// did not change when the transport moved, and two parsers for one
    /// payload would be two chances for a half-migrated fleet to disagree
    /// about what a route means.
    fn accept(&self, body: &[u8]) -> Result<RoutingTableDocument, FetchError> {
        let document: RoutingTableDocument = serde_json::from_slice(body)?;
        document.validate()?;
        debug!(
            version = %document.version,
            routes = document.routes.len(),
            environments = document.environments.len(),
            bytes = body.len(),
            transport = self.transport(),
            "routing table fetched"
        );
        Ok(document)
    }

    async fn fetch_over_machine_channel(
        &self,
        connect: &dyn MachineConnect,
    ) -> Result<RoutingTableDocument, FetchError> {
        let channel = connect.connect().await?;
        // The ceiling is enforced by the decoder rather than after the fact,
        // so an answer larger than the platform can plausibly have is refused
        // while it is still arriving instead of being buffered whole first.
        let mut client =
            MachineChannelClient::new(channel).max_decoding_message_size(self.max_table_bytes);

        let proxy_id = self.proxy_id.clone().unwrap_or_default();
        let answer = client
            .fetch_routing_table(FetchRoutingTableRequest {
                proxy_id: proxy_id.clone(),
                observed_routing_table_version: self.observed().unwrap_or_default(),
            })
            .await
            .map_err(|status| match status.code() {
                // The one refusal an operator can act on, and the likeliest
                // one during the migration: this proxy asked for a table as a
                // proxy id its certificate does not name.
                tonic::Code::PermissionDenied => FetchError::Denied {
                    proxy_id,
                    message: status.message().to_string(),
                },
                // `OutOfRange` is what tonic's own decoder reports when a
                // message exceeds the limit set above, and this RPC has no
                // out-of-range condition of its own to confuse it with, so
                // the ceiling is reported in the words that say which setting
                // raises it.
                tonic::Code::OutOfRange => FetchError::TooLarge {
                    limit: self.max_table_bytes,
                },
                code => FetchError::Rpc {
                    code: format!("{code:?}"),
                    message: status.message().to_string(),
                },
            })?;

        self.accept(answer.into_inner().routing_table_json.as_bytes())
    }
}

/// Build the client this configuration asks for: the machine channel, proving
/// the identity this proxy was given. There is no fallback for a proxy with
/// none - the routing table is served nowhere else.
///
/// The choice lives here rather than at the call site in `main` so that it is
/// one function with one rule in it, and so a test can ask what a given
/// configuration produces without starting a proxy. It is also the one place
/// that knows the id this proxy asks for a table as: `control.associated_id`,
/// the same one the control plane checks against the name the connection
/// proved before it answers at all.
pub fn routing_table_client(
    control: &ControlConfig,
    identity: Option<(&Arc<MachineIdentity>, &MachineIdentityConfig)>,
    max_table_bytes: usize,
) -> Result<CatalogClient, FetchError> {
    let client = match identity {
        Some((identity, config)) => {
            let expected =
                config
                    .expected_control_identity()
                    .map_err(|error| FetchError::Address {
                        addr: config.control_endpoint.clone(),
                        reason: format!("the control plane's expected name is unusable: {error}"),
                    })?;
            CatalogClient::over_machine_channel(
                config.control_endpoint.clone(),
                Arc::clone(identity),
                expected,
                control.request_timeout_duration(),
                max_table_bytes,
            )
        }
        // Unreachable through a configuration this build accepts:
        // `SystemConfig::validate` refuses a proxy that names a control
        // plane without a [machine_identity] table, because the routing
        // table is served nowhere else. Kept as an error rather than an
        // `unreachable!()` so a caller assembling a client by hand gets an
        // answer instead of a panic.
        None => {
            return Err(FetchError::Address {
                addr: control.addr.clone(),
                reason: "this proxy has no [machine_identity] table, so it can prove nothing \
                         to the control plane and the routing table is served nowhere else"
                    .to_string(),
            });
        }
    };
    Ok(client.with_proxy_id(control.associated_id.clone()))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use deis_identity::names;
    use deis_identity_test_pki::{TestCa, write_files};
    use deis_wire::machine::FetchRoutingTableResponse;
    use deis_wire::machine::machine_channel_server::{MachineChannel, MachineChannelServer};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{Request, Response, Status};

    use super::*;

    const V1: &str = r#"{"version":"v1","generated_at":11,"routes":[
        {"app_id":"shop","environment":"production","host":"shop.test","path_prefix":"/","priority":0}
    ],"environments":[]}"#;

    /// A stand-in for the control plane's machine channel: it answers with
    /// whatever the test set and keeps what the last request carried.
    ///
    /// Served without TLS, over a connector this test substitutes for the
    /// dialler. That is the seam described on [`MachineConnect`]: the
    /// handshake is deis-identity's and is tested against the real listener
    /// in deis-control, while what is in question here is the request this
    /// module sends and what it does with the answer.
    struct MockMachineChannel {
        answer: Mutex<Result<String, Status>>,
        last: Mutex<Option<FetchRoutingTableRequest>>,
    }

    /// The handle the generated server is given. A newtype rather than
    /// `Arc<MockMachineChannel>` itself because the trait and `Arc` are both
    /// foreign to this crate.
    #[derive(Clone)]
    struct MockService(Arc<MockMachineChannel>);

    #[tonic::async_trait]
    impl MachineChannel for MockService {
        async fn register(
            &self,
            _request: Request<deis_wire::machine::RegisterNodeRequest>,
        ) -> Result<Response<deis_wire::machine::RegisterNodeResponse>, Status> {
            Err(Status::unimplemented("a proxy never registers as a node"))
        }

        async fn deregister(
            &self,
            _request: Request<deis_wire::machine::DeregisterNodeRequest>,
        ) -> Result<Response<deis_wire::machine::DeregisterNodeResponse>, Status> {
            Err(Status::unimplemented("a proxy never registers as a node"))
        }

        async fn fetch_assignments(
            &self,
            _request: Request<deis_wire::machine::FetchAssignmentsRequest>,
        ) -> Result<Response<deis_wire::machine::FetchAssignmentsResponse>, Status> {
            Err(Status::unimplemented("this proxy never asks for these"))
        }

        async fn fetch_routing_table(
            &self,
            request: Request<FetchRoutingTableRequest>,
        ) -> Result<Response<FetchRoutingTableResponse>, Status> {
            *self.0.last.lock().unwrap() = Some(request.into_inner());
            match self.0.answer.lock().unwrap().clone() {
                Ok(routing_table_json) => Ok(Response::new(FetchRoutingTableResponse {
                    routing_table_json,
                })),
                Err(status) => Err(status),
            }
        }

        async fn fetch_secret_values(
            &self,
            _request: Request<deis_wire::machine::FetchSecretValuesRequest>,
        ) -> Result<Response<deis_wire::machine::FetchSecretValuesResponse>, Status> {
            Err(Status::unimplemented("fetch_secret_values"))
        }

        async fn issue_capability(
            &self,
            _request: Request<deis_wire::machine::IssueCapabilityRequest>,
        ) -> Result<Response<deis_wire::machine::IssueCapabilityResponse>, Status> {
            Err(Status::unimplemented("issue_capability"))
        }
    }

    /// A plain HTTP/2 connection to the mock above, in place of the mutual
    /// TLS one.
    struct PlainConnection(String);

    #[async_trait]
    impl MachineConnect for PlainConnection {
        async fn connect(&self) -> Result<Channel, FetchError> {
            Endpoint::from_shared(self.0.clone())
                .map_err(|error| FetchError::Transport(error.to_string()))?
                .connect()
                .await
                .map_err(|error| FetchError::Transport(error.to_string()))
        }
    }

    struct Mock {
        state: Arc<MockMachineChannel>,
        uri: String,
    }

    impl Mock {
        async fn start(answer: Result<String, Status>) -> Self {
            let state = Arc::new(MockMachineChannel {
                answer: Mutex::new(answer),
                last: Mutex::new(None),
            });
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let uri = format!("http://{}", listener.local_addr().unwrap());
            let served = Arc::clone(&state);
            tokio::spawn(async move {
                let _ = tonic::transport::Server::builder()
                    .add_service(MachineChannelServer::new(MockService(served)))
                    .serve_with_incoming(TcpListenerStream::new(listener))
                    .await;
            });
            Self { state, uri }
        }

        fn client(&self, max_table_bytes: usize) -> CatalogClient {
            CatalogClient::with_transport(
                Transport::Machine {
                    authority: self.uri.clone(),
                    connect: Box::new(PlainConnection(self.uri.clone())),
                },
                Duration::from_secs(5),
                max_table_bytes,
            )
        }

        fn last(&self) -> FetchRoutingTableRequest {
            self.state
                .last
                .lock()
                .unwrap()
                .clone()
                .expect("the mock was never called")
        }
    }

    #[tokio::test]
    async fn an_rpc_answer_is_read_by_the_document_parser_the_http_path_uses() {
        let mock = Mock::start(Ok(V1.to_string())).await;
        let document = mock.client(1024 * 1024).fetch().await.expect("a table");
        assert_eq!(document.version, "v1");
        assert_eq!(document.routes.len(), 1);
        assert_eq!(document.routes[0].app_id, "shop");
        assert_eq!(document.routes[0].host.as_deref(), Some("shop.test"));
    }

    #[tokio::test]
    async fn the_headers_that_became_fields_carry_what_the_headers_carried() {
        let mock = Mock::start(Ok(V1.to_string())).await;
        let client = mock
            .client(1024 * 1024)
            .with_proxy_id("proxy-a".to_string());

        client.fetch().await.expect("a table");
        let first = mock.last();
        assert_eq!(first.proxy_id, "proxy-a");
        assert_eq!(
            first.observed_routing_table_version, "",
            "receipt of the first answer is not yet an applied observation, and machine.proto \
             says an empty field is how a caller says it has applied nothing"
        );

        // What the poller does once ConfigManager has published the table.
        client.observe_routing_table("v1".to_string());
        client.fetch().await.expect("a table");
        assert_eq!(mock.last().observed_routing_table_version, "v1");
    }

    #[tokio::test]
    async fn a_proxy_asking_as_someone_else_is_told_which_two_settings_disagree() {
        let mock = Mock::start(Err(Status::permission_denied(
            "this connection proved proxy-b",
        )))
        .await;
        let error = mock
            .client(1024 * 1024)
            .with_proxy_id("proxy-a".to_string())
            .fetch()
            .await
            .expect_err("a refusal is not a table");
        assert!(matches!(error, FetchError::Denied { .. }), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("proxy-a"), "{message}");
        assert!(message.contains("control.associated_id"), "{message}");
    }

    #[tokio::test]
    async fn a_table_larger_than_the_ceiling_is_refused_on_this_transport_too() {
        // Large enough that the ceiling below cannot admit it, and valid, so
        // that what refuses it is the size and not the content.
        let mock = Mock::start(Ok(V1.to_string())).await;
        let error = mock
            .client(32)
            .fetch()
            .await
            .expect_err("a table over the ceiling is not a table");
        assert!(
            matches!(error, FetchError::TooLarge { limit: 32 }),
            "a decoder refusal has to arrive as the ceiling it is, naming the setting that \
             raises it: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_document_that_does_not_validate_is_refused_whole_on_this_transport_too() {
        let mock = Mock::start(Ok(
            r#"{"version":"","routes":[],"environments":[]}"#.to_string()
        ))
        .await;
        let error = mock
            .client(1024 * 1024)
            .fetch()
            .await
            .expect_err("no table");
        assert!(matches!(error, FetchError::Invalid(_)), "{error:?}");

        let mock = Mock::start(Ok("not json at all".to_string())).await;
        let error = mock
            .client(1024 * 1024)
            .fetch()
            .await
            .expect_err("no table");
        assert!(matches!(error, FetchError::Malformed(_)), "{error:?}");
    }

    #[tokio::test]
    async fn an_unanswerable_call_is_a_failure_and_not_an_empty_table() {
        // The dangerous reading of any refusal is "the control plane has no
        // routes", which would silently unroute the whole platform.
        let mock = Mock::start(Err(Status::internal("the store is down"))).await;
        let error = mock
            .client(1024 * 1024)
            .fetch()
            .await
            .expect_err("no table");
        assert!(matches!(error, FetchError::Rpc { .. }), "{error:?}");
        assert!(error.to_string().contains("Internal"), "{error}");
    }

    fn identity(directory: &std::path::Path) -> Arc<MachineIdentity> {
        let ca = TestCa::new();
        let id = names::proxy("deis.internal", "edge-1").expect("a proxy name");
        let files = write_files(directory, "proxy", &ca.issue(&id), &ca.root_pem);
        Arc::new(MachineIdentity::load(files.as_identity_files(), &[]).expect("an identity"))
    }

    fn control() -> ControlConfig {
        ControlConfig::new(
            "edge-1".to_string(),
            "http://control.internal:8080".to_string(),
        )
    }

    #[test]
    fn an_identity_moves_the_fetch_onto_the_machine_channel() {
        let directory = tempfile::tempdir().expect("a directory");
        let identity = identity(directory.path());
        let config = MachineIdentityConfig {
            certificate: "ignored: the identity above is already loaded".to_string(),
            private_key: "ignored".to_string(),
            trust_root: "ignored".to_string(),
            trust_domain: "deis.internal".to_string(),
            control_endpoint: "https://control.internal:8443".to_string(),
            ..Default::default()
        };

        let client = routing_table_client(&control(), Some((&identity, &config)), 1024).unwrap();
        assert_eq!(client.transport(), "machine channel");
        assert_eq!(
            client.authority(),
            "control.internal:8443",
            "the machine channel is a port of its own, not [control] addr"
        );
        assert_eq!(client.path(), FETCH_ROUTING_TABLE_METHOD);
        assert_eq!(
            client.proxy_id.as_deref(),
            Some("edge-1"),
            "the control plane refuses a proxy_id that is not the one the connection proved, and \
             control.associated_id is what this proxy is registered as"
        );
    }

    #[test]
    fn an_identity_whose_control_name_cannot_be_built_is_reported_rather_than_dialled() {
        // Unreachable through a validated configuration - `[machine_identity]`
        // refuses this at startup - but the two checks are in different
        // files, and a client that quietly dialled a nameless peer would be
        // trusting whatever certificate answered.
        let directory = tempfile::tempdir().expect("a directory");
        let identity = identity(directory.path());
        let config = MachineIdentityConfig {
            trust_domain: String::new(),
            control_endpoint: "https://control.internal:8443".to_string(),
            ..Default::default()
        };
        // Matched rather than unwrapped: a `CatalogClient` holds a connector
        // and a boxed dialler, neither of which has a `Debug`, so the success
        // case cannot be printed by `expect_err`.
        let error = match routing_table_client(&control(), Some((&identity, &config)), 1024) {
            Ok(_) => panic!("a peer with no name is not a peer"),
            Err(error) => error,
        };
        assert!(matches!(error, FetchError::Address { .. }), "{error:?}");
    }
}
