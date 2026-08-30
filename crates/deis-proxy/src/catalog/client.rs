//! Fetching the routing table from the control plane over HTTP.
//!
//! The connection is made with pingora's own HTTP connector - the same
//! machinery the data plane uses to reach an upstream - so this adds no
//! second HTTP stack to the process and inherits its TLS behaviour,
//! certificate verification included.
//!
//! One connection is opened per poll and dropped when the table has been
//! read. Nothing is pooled on purpose: a poll happens every few seconds and
//! a pooled connection to the control plane would be one more socket to
//! carry across a zero-downtime upgrade for no measurable saving.
//!
//! The whole exchange is under one timeout. A control plane that accepts the
//! TCP connection and then says nothing is the failure mode that would
//! otherwise park the poll loop for ever, and a parked poll loop is a data
//! plane that stops noticing route changes while looking perfectly healthy.

use std::net::SocketAddr;
use std::time::Duration;

use pingora::connectors::http::Connector;
use pingora::http::RequestHeader;
use pingora::upstreams::peer::HttpPeer;
use tokio::time::timeout;
use tracing::debug;

use super::wire::{DocumentError, ROUTING_TABLE_PATH, RoutingTableDocument};

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

/// Reads the routing table. One of these lives for the life of the process.
pub struct CatalogClient {
    endpoint: ControlEndpoint,
    /// A `node_preshared_keys` value: the same credential the gRPC control
    /// channel presents. See `deis-control`'s `api::catalog::routing_table`
    /// for why the table is on that channel rather than the human API.
    credential: String,
    request_timeout: Duration,
    max_table_bytes: usize,
    connector: Connector,
}

impl CatalogClient {
    pub fn new(
        endpoint: ControlEndpoint,
        credential: String,
        request_timeout: Duration,
        max_table_bytes: usize,
    ) -> Self {
        Self {
            endpoint,
            credential,
            request_timeout,
            max_table_bytes,
            connector: Connector::new(None),
        }
    }

    pub fn endpoint(&self) -> &ControlEndpoint {
        &self.endpoint
    }

    /// One complete fetch, or an error. Nothing partial is ever returned:
    /// the document is parsed and validated whole before it is handed back.
    pub async fn fetch(&self) -> Result<RoutingTableDocument, FetchError> {
        match timeout(self.request_timeout, self.fetch_inner()).await {
            Ok(result) => result,
            Err(_) => Err(FetchError::Timeout(self.request_timeout)),
        }
    }

    async fn fetch_inner(&self) -> Result<RoutingTableDocument, FetchError> {
        let authority = self.endpoint.authority();
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

        let sni = if self.endpoint.tls {
            self.endpoint.host.clone()
        } else {
            String::new()
        };
        let mut peer = HttpPeer::new(addr, self.endpoint.tls, sni);
        peer.options.connection_timeout = Some(self.request_timeout);
        peer.options.read_timeout = Some(self.request_timeout);
        peer.options.write_timeout = Some(self.request_timeout);

        let (mut session, _reused) = self
            .connector
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
            &format!("Bearer {}", self.credential),
        )?;

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

        let document: RoutingTableDocument = serde_json::from_slice(&body)?;
        document.validate()?;
        debug!(
            version = %document.version,
            routes = document.routes.len(),
            environments = document.environments.len(),
            bytes = body.len(),
            "routing table fetched"
        );
        Ok(document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
