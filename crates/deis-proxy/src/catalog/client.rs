//! Fetching the routing table from the control plane, over whichever channel
//! this proxy is configured to reach it on.
//!
//! # Two transports, one document
//!
//! Without a `[machine_identity]` table, the routing table is fetched with an
//! HTTP GET on the control plane's human listener, authenticated with the
//! fleet-wide `node_preshared_keys` value - a secret every process in the
//! fleet holds a copy of. With one, the same table is fetched as
//! `deis.machine.MachineChannel/FetchRoutingTable` on the machine channel,
//! authenticated by a certificate that names this proxy and nothing else, and
//! answered by a control plane that has proved its own name back.
//!
//! Only the transport differs. The document is the same JSON either way and
//! is parsed and validated by the same [`super::wire::RoutingTableDocument`]
//! code, under the same all-or-nothing rule, so a platform halfway through
//! the migration cannot end up running two subtly different notions of what a
//! route is. The two values that used to be request headers - which proxy is
//! asking, and which version of the table it is already running - are fields
//! on the RPC and carry exactly what the headers carried.
//!
//! # What each transport costs the process
//!
//! The HTTP path is made with pingora's own connector, the same machinery the
//! data plane uses to reach an upstream, so it adds no HTTP stack of its own
//! and inherits pingora's TLS behaviour, certificate verification included.
//!
//! The machine channel is tonic over hyper, and that is not a second stack
//! this module introduces either: deis-wire's control client is tonic, so
//! this binary has linked tonic and hyper since the day it grew a control
//! channel - `cargo tree -p deis-proxy -e normal -i tonic` shows the edge, and
//! `-i hyper` shows that the only hyper in the tree comes in behind it, not
//! behind pingora. What is new here is that the catalog poller uses them too,
//! not that they are present.
//!
//! One connection is opened per poll and dropped when the table has been
//! read, on both transports. Nothing is pooled on purpose: a poll happens
//! every few seconds, and a pooled connection to the control plane would be
//! one more socket to carry across a zero-downtime upgrade for no measurable
//! saving. On the machine channel it has a second effect worth having - the
//! handshake is made again on every poll, so a certificate renewed on disk is
//! presented by the next poll without anything having to be told it changed.
//!
//! The whole exchange is under one timeout on both. A control plane that
//! accepts the TCP connection and then says nothing is the failure mode that
//! would otherwise park the poll loop for ever, and a parked poll loop is a
//! data plane that stops noticing route changes while looking perfectly
//! healthy. [`CatalogClient::fetch`] is the bound, and it wraps either
//! transport whole - connect, handshake, request and body alike. The gRPC
//! path hands tonic the same duration again as its connect timeout and its
//! per-request timeout, which changes no guarantee but reports the ordinary
//! failures as what they are instead of delivering all of them as one
//! stopwatch running out.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use deis_identity::{Dialler, MachineIdentity, Protocol};
use deis_wire::ControlConfig;
use deis_wire::machine::FetchRoutingTableRequest;
use deis_wire::machine::machine_channel_client::MachineChannelClient;
use pingora::connectors::http::Connector;
use pingora::http::RequestHeader;
use pingora::upstreams::peer::HttpPeer;
use tokio::time::timeout;
use tonic::transport::{Channel, Endpoint};
use tracing::debug;

use super::wire::{DocumentError, ROUTING_TABLE_PATH, RoutingTableDocument};
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

/// Where the control plane is and how to prove we may ask it.
#[derive(Debug, Clone)]
pub struct ControlEndpoint {
    /// Host as written in the configuration, used for the `Host` header and
    /// as the TLS server name.
    host: String,
    port: u16,
    tls: bool,
}

impl ControlEndpoint {
    /// Split a `http://host:port` / `https://host:port` address, the form
    /// `deis_wire::ControlConfig` validates.
    pub fn parse(addr: &str) -> Result<Self, FetchError> {
        let reject = |reason: &str| FetchError::Address {
            addr: addr.to_string(),
            reason: reason.to_string(),
        };
        let (tls, rest) = if let Some(rest) = addr.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = addr.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(reject("it must start with http:// or https://"));
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        if authority.is_empty() {
            return Err(reject("it names no host"));
        }
        // An IPv6 literal is bracketed; anything else splits on the last
        // colon, so a bare `host` keeps the scheme's default port.
        let (host, port) = if let Some(end) = authority.strip_prefix('[') {
            let mut parts = end.splitn(2, ']');
            let host = parts.next().unwrap_or_default().to_string();
            let port = match parts.next().and_then(|p| p.strip_prefix(':')) {
                Some(port) => port
                    .parse()
                    .map_err(|_| reject("the port after the IPv6 literal is not a number"))?,
                None => default_port(tls),
            };
            (host, port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (
                    host.to_string(),
                    port.parse()
                        .map_err(|_| reject("the part after the last ':' is not a port number"))?,
                ),
                None => (authority.to_string(), default_port(tls)),
            }
        };
        if host.is_empty() {
            return Err(reject("it names no host"));
        }
        Ok(Self { host, port, tls })
    }

    /// `host:port`, as it goes in the `Host` header and into the resolver.
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn default_port(tls: bool) -> u16 {
    if tls { 443 } else { 80 }
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
    /// HTTP on the control plane's human listener, with the fleet secret.
    Http {
        endpoint: ControlEndpoint,
        /// A `node_preshared_keys` value: the same credential the gRPC
        /// control channel presents. See `deis-control`'s
        /// `api::catalog::routing_table` for why the table is on that channel
        /// rather than the human API.
        credential: String,
        // Boxed because pingora's connector carries its whole pool
        // configuration inline - some 450 bytes - and an enum is as large as
        // its largest variant, so leaving it unboxed would make every
        // machine-channel client pay for a pool it does not have.
        connector: Box<Connector>,
    },
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
    /// Fetch over HTTP, with the fleet secret. What a proxy with no
    /// `[machine_identity]` table does, unchanged.
    pub fn new(
        endpoint: ControlEndpoint,
        credential: String,
        request_timeout: Duration,
        max_table_bytes: usize,
    ) -> Self {
        Self::with_transport(
            Transport::Http {
                endpoint,
                credential,
                connector: Box::new(Connector::new(None)),
            },
            request_timeout,
            max_table_bytes,
        )
    }

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
            Transport::Http { endpoint, .. } => endpoint.authority(),
            Transport::Machine { authority, .. } => authority.clone(),
        }
    }

    /// Which channel this client fetches on, in the words a log line has room
    /// for. Also what a test asserts on to check the choice was made the way
    /// the configuration asked for.
    pub fn transport(&self) -> &'static str {
        match &self.transport {
            Transport::Http { .. } => "http",
            Transport::Machine { .. } => "machine channel",
        }
    }

    /// What it asks for there: an HTTP route on one transport, a gRPC method
    /// on the other.
    pub fn path(&self) -> &'static str {
        match &self.transport {
            Transport::Http { .. } => ROUTING_TABLE_PATH,
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
            match &self.transport {
                Transport::Http {
                    endpoint,
                    credential,
                    connector,
                } => self.fetch_over_http(endpoint, credential, connector).await,
                Transport::Machine { connect, .. } => {
                    self.fetch_over_machine_channel(connect.as_ref()).await
                }
            }
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

    async fn fetch_over_http(
        &self,
        endpoint: &ControlEndpoint,
        credential: &str,
        connector: &Connector,
    ) -> Result<RoutingTableDocument, FetchError> {
        let authority = endpoint.authority();
        let addr: SocketAddr = tokio::net::lookup_host(authority.as_str())
            .await
            .map_err(|source| FetchError::Resolve {
                addr: authority.clone(),
                source,
            })?
            .next()
            .ok_or_else(|| FetchError::Address {
                addr: authority.clone(),
                reason: "it resolved to no address".to_string(),
            })?;

        let sni = if endpoint.tls {
            endpoint.host.clone()
        } else {
            String::new()
        };
        let mut peer = HttpPeer::new(addr, endpoint.tls, sni);
        peer.options.connection_timeout = Some(self.request_timeout);
        peer.options.read_timeout = Some(self.request_timeout);
        peer.options.write_timeout = Some(self.request_timeout);

        let (mut session, _reused) = connector
            .get_http_session(&peer)
            .await
            .map_err(|e| FetchError::Transport(format!("connecting to {authority}: {e}")))?;

        let mut request = RequestHeader::build("GET", ROUTING_TABLE_PATH.as_bytes(), None)
            .map_err(|e| FetchError::Transport(format!("building the request: {e}")))?;
        let set = |request: &mut RequestHeader, name: &str, value: &str| {
            request
                .insert_header(name.to_string(), value)
                .map_err(|e| FetchError::Transport(format!("setting {name}: {e}")))
        };
        set(&mut request, "Host", &authority)?;
        set(&mut request, "Accept", "application/json")?;
        set(
            &mut request,
            "Authorization",
            &format!("Bearer {credential}"),
        )?;
        if let Some(proxy_id) = &self.proxy_id {
            set(
                &mut request,
                deis_wire::conventions::PROXY_ID_HEADER,
                proxy_id,
            )?;
        }
        if let Some(version) = self.observed().as_deref() {
            set(
                &mut request,
                deis_wire::conventions::OBSERVED_ROUTING_TABLE_HEADER,
                version,
            )?;
        }

        session
            .write_request_header(Box::new(request))
            .await
            .map_err(|e| FetchError::Transport(format!("sending the request: {e}")))?;
        session
            .finish_request_body()
            .await
            .map_err(|e| FetchError::Transport(format!("finishing the request: {e}")))?;
        session
            .read_response_header()
            .await
            .map_err(|e| FetchError::Transport(format!("reading the response header: {e}")))?;

        let header = session
            .response_header()
            .ok_or_else(|| FetchError::Transport("the response carried no header".to_string()))?;
        let status = header.status.as_u16();
        let declared: Option<usize> = header
            .headers
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok());

        let mut body = Vec::new();
        while let Some(chunk) = session
            .read_response_body()
            .await
            .map_err(|e| FetchError::Transport(format!("reading the response body: {e}")))?
        {
            if body.len() + chunk.len() > self.max_table_bytes {
                return Err(FetchError::TooLarge {
                    limit: self.max_table_bytes,
                });
            }
            body.extend_from_slice(&chunk);
        }

        if status != 200 {
            let detail = String::from_utf8_lossy(&body[..body.len().min(200)]).to_string();
            return Err(FetchError::Status {
                status,
                path: ROUTING_TABLE_PATH,
                detail: if detail.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", detail.trim())
                },
            });
        }

        // A body that stopped early is the one way a half-written table
        // could reach the parser and still be valid JSON prefix-wise, so it
        // is checked against what the control plane said it was sending
        // rather than trusted to fail later.
        if let Some(declared) = declared
            && body.len() < declared
        {
            return Err(FetchError::Truncated {
                got: body.len(),
                declared,
            });
        }

        self.accept(&body)
    }
}

/// Build the client this configuration asks for: the machine channel if this
/// proxy has an identity, HTTP with the fleet secret if it has not.
///
/// The choice lives here rather than at the call site in `main` so that it is
/// one function with one rule in it, and so a test can ask what a given
/// configuration produces without starting a proxy. It is also the one place
/// that knows the id this proxy asks for a table as: `control.associated_id`,
/// the same value the HTTP path has always sent in the `x-deis-proxy-id`
/// header, and the same one the control plane checks against the name the
/// connection proved before it answers at all.
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
        // No identity, so the only thing left to prove with is the fleet
        // secret - and a proxy that has neither cannot ask for the table at
        // all. Saying so here, by name, beats sending an empty bearer value
        // and reporting whatever the control plane answers to it.
        None => CatalogClient::new(
            ControlEndpoint::parse(&control.addr)?,
            control.secret.clone().ok_or_else(|| FetchError::Address {
                addr: control.addr.clone(),
                reason: "this proxy has neither a [machine_identity] table nor a [control] secret,                          so it can prove nothing to the control plane and the routing table cannot                          be fetched. Give it an identity, which names this one proxy, or a secret,                          which the whole fleet shares"
                    .to_string(),
            })?,
            control.request_timeout_duration(),
            max_table_bytes,
        ),
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

    #[test]
    fn addresses_are_split_into_host_port_and_scheme() {
        let plain = ControlEndpoint::parse("http://127.0.0.1:8085").unwrap();
        assert_eq!(plain.authority(), "127.0.0.1:8085");
        assert!(!plain.tls);

        let tls = ControlEndpoint::parse("https://control.internal").unwrap();
        assert_eq!(tls.authority(), "control.internal:443");
        assert!(tls.tls);

        assert_eq!(
            ControlEndpoint::parse("http://control.internal")
                .unwrap()
                .authority(),
            "control.internal:80"
        );
        assert_eq!(
            ControlEndpoint::parse("http://[::1]:8085")
                .unwrap()
                .authority(),
            "[::1]:8085"
        );
        // A trailing path is not part of the authority.
        assert_eq!(
            ControlEndpoint::parse("http://host:9/anything")
                .unwrap()
                .authority(),
            "host:9"
        );
    }

    #[test]
    fn unusable_addresses_are_refused_rather_than_guessed() {
        for bad in ["", "127.0.0.1:8085", "http://", "http://host:port"] {
            assert!(
                ControlEndpoint::parse(bad).is_err(),
                "accepted {bad:?}, which would have been dialled as something else"
            );
        }
    }

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
            "fleet-secret".to_string(),
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
    fn no_identity_leaves_the_fetch_exactly_where_it_was() {
        let client = routing_table_client(&control(), None, 1024).unwrap();
        assert_eq!(client.transport(), "http");
        assert_eq!(client.authority(), "control.internal:8080");
        assert_eq!(client.path(), ROUTING_TABLE_PATH);
        assert_eq!(client.proxy_id.as_deref(), Some("edge-1"));
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
