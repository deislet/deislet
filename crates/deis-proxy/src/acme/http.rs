//! The HTTP hop to the certificate authority.
//!
//! Pingora's own connector, the same one the data plane uses to reach a node
//! and the same one [`crate::catalog`] reads the routing table with, so this
//! adds no second HTTP stack to the process and inherits its TLS behaviour -
//! including verifying the authority's certificate, which for a client that
//! exists to obtain certificates would be an embarrassing thing to skip.
//!
//! Nothing is pooled. A renewal pass makes a handful of requests every
//! twelve hours; a connection kept open between them would be one more
//! socket to think about across a zero-downtime upgrade for no saving worth
//! measuring.

use std::net::SocketAddr;
use std::time::Duration;

use pingora::connectors::http::Connector;
use pingora::http::RequestHeader;
use pingora::protocols::http::client::HttpSession;
use pingora::upstreams::peer::HttpPeer;
use tokio::time::timeout;

use super::error::AcmeError;

/// Ceiling on one response body from the authority.
///
/// A certificate chain is a few kilobytes and a problem document less; this
/// is generous by three orders of magnitude and exists only so that a
/// misbehaving or hostile endpoint cannot stream this process out of memory
/// while it waits for a certificate.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// An absolute URL split into the parts a request needs.
#[derive(Debug, Clone, PartialEq)]
pub struct AcmeUrl {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    /// Path and query, always beginning with `/`.
    pub path: String,
}

impl AcmeUrl {
    pub fn parse(url: &str) -> Result<Self, AcmeError> {
        let reject = |reason: &str| {
            Err(AcmeError::protocol(
                format!("the URL {url:?}"),
                reason.to_string(),
            ))
        };
        let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return reject("it must start with http:// or https://");
        };

        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(index) => (&rest[..index], rest[index..].to_string()),
            None => (rest, "/".to_string()),
        };
        // A URL with a query but no path (`host?x=1`) still has to send a
        // path; a request line that started with `?` is not a request line.
        let path = if path.starts_with('/') {
            path
        } else {
            format!("/{path}")
        };
        if authority.is_empty() {
            return reject("it names no host");
        }

        let (host, port) = if let Some(end) = authority.strip_prefix('[') {
            let mut parts = end.splitn(2, ']');
            let host = parts.next().unwrap_or_default().to_string();
            let port = match parts.next().and_then(|rest| rest.strip_prefix(':')) {
                Some(port) => match port.parse() {
                    Ok(port) => port,
                    Err(_) => return reject("the port after the IPv6 literal is not a number"),
                },
                None => default_port(tls),
            };
            (host, port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => match port.parse() {
                    Ok(port) => (host.to_string(), port),
                    Err(_) => return reject("the part after the last ':' is not a port number"),
                },
                None => (authority.to_string(), default_port(tls)),
            }
        };
        if host.is_empty() {
            return reject("it names no host");
        }
        Ok(Self {
            host,
            port,
            tls,
            path,
        })
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

/// One answer from the authority.
pub struct AcmeResponse {
    pub status: u16,
    /// Header names lowercased, in arrival order.
    pub(super) headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl AcmeResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The `Retry-After` the authority asked for, when it sent one as a
    /// delay in seconds. The HTTP-date form is not read: RFC 8555 authorities
    /// send seconds, and guessing at a date badly would be worse than falling
    /// back to the caller's own interval.
    pub fn retry_after(&self) -> Option<Duration> {
        self.header("retry-after")
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
    }
}

/// A client for the handful of requests an ACME exchange makes.
pub struct AcmeHttp {
    connector: Connector,
    request_timeout: Duration,
}

impl AcmeHttp {
    pub fn new(request_timeout: Duration) -> Self {
        Self {
            connector: Connector::new(None),
            request_timeout,
        }
    }

    pub async fn head(&self, url: &str) -> Result<AcmeResponse, AcmeError> {
        self.request("HEAD", url, None).await
    }

    pub async fn get(&self, url: &str) -> Result<AcmeResponse, AcmeError> {
        self.request("GET", url, None).await
    }

    /// A signed ACME request. The content type is fixed by RFC 8555 section
    /// 6.2 and an authority refuses anything else.
    pub async fn post_jose(&self, url: &str, body: &str) -> Result<AcmeResponse, AcmeError> {
        self.request("POST", url, Some(body)).await
    }

    async fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
    ) -> Result<AcmeResponse, AcmeError> {
        match timeout(self.request_timeout, self.request_inner(method, url, body)).await {
            Ok(result) => result,
            Err(_) => Err(AcmeError::Timeout(self.request_timeout)),
        }
    }

    /// Connect to the first of `addrs` that accepts, in the order the
    /// resolver gave them.
    ///
    /// An authority publishes both address families, and a host with an
    /// IPv6 route that is configured and broken sees every connection to the
    /// first address time out. Taking only the first address made every
    /// request fail after `request_timeout` with no fallback to the address
    /// that would have worked; the failure line named the authority and
    /// nothing else. Each address gets one attempt, and when none accepts
    /// the error lists every attempt so the broken route is visible.
    async fn connect_any(
        &self,
        authority: &str,
        addrs: &[SocketAddr],
        tls: bool,
        sni: &str,
    ) -> Result<HttpSession, AcmeError> {
        let mut failures = Vec::new();
        for addr in addrs {
            let mut peer = HttpPeer::new(*addr, tls, sni.to_string());
            peer.options.connection_timeout = Some(self.request_timeout);
            peer.options.read_timeout = Some(self.request_timeout);
            peer.options.write_timeout = Some(self.request_timeout);
            match self.connector.get_http_session(&peer).await {
                Ok((session, _reused)) => return Ok(session),
                Err(e) => {
                    if addrs.len() > 1 {
                        tracing::debug!(
                            authority,
                            %addr,
                            error = %e,
                            "one of the authority's addresses did not accept a connection; \
                             trying the next"
                        );
                    }
                    failures.push(format!("{addr}: {e}"));
                }
            }
        }
        Err(AcmeError::transport(
            format!("connecting to {authority}"),
            failures.join(" | "),
        ))
    }

    async fn request_inner(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
    ) -> Result<AcmeResponse, AcmeError> {
        let target = AcmeUrl::parse(url)?;
        let authority = target.authority();
        let context = format!("{method} {url}");

        let addrs: Vec<SocketAddr> = tokio::net::lookup_host(authority.as_str())
            .await
            .map_err(|e| AcmeError::transport(format!("resolving {authority}"), e))?
            .collect();
        if addrs.is_empty() {
            return Err(AcmeError::transport(
                format!("resolving {authority}"),
                "it resolved to no address",
            ));
        }

        let sni = if target.tls {
            target.host.clone()
        } else {
            String::new()
        };
        let mut session = self
            .connect_any(&authority, &addrs, target.tls, &sni)
            .await?;

        let mut request = RequestHeader::build(method, target.path.as_bytes(), None)
            .map_err(|e| AcmeError::transport(context.clone(), e))?;
        let mut set = |name: &str, value: &str| {
            request
                .insert_header(name.to_string(), value)
                .map_err(|e| AcmeError::transport(format!("setting {name}"), e))
        };
        set("Host", &authority)?;
        set(
            "User-Agent",
            concat!("deis-proxy/", env!("CARGO_PKG_VERSION")),
        )?;
        set("Accept", "*/*")?;
        if let Some(body) = body {
            set("Content-Type", "application/jose+json")?;
            set("Content-Length", &body.len().to_string())?;
        }

        session
            .write_request_header(Box::new(request))
            .await
            .map_err(|e| AcmeError::transport(format!("sending {context}"), e))?;
        if let Some(body) = body {
            session
                .write_request_body(bytes::Bytes::copy_from_slice(body.as_bytes()), true)
                .await
                .map_err(|e| AcmeError::transport(format!("sending the body of {context}"), e))?;
        }
        session
            .finish_request_body()
            .await
            .map_err(|e| AcmeError::transport(format!("finishing {context}"), e))?;
        session
            .read_response_header()
            .await
            .map_err(|e| AcmeError::transport(format!("reading the answer to {context}"), e))?;

        let header = session.response_header().ok_or_else(|| {
            AcmeError::protocol(context.clone(), "the response carried no header")
        })?;
        let status = header.status.as_u16();
        let headers: Vec<(String, String)> = header
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();

        let mut collected = Vec::new();
        while let Some(chunk) = session.read_response_body().await.map_err(|e| {
            AcmeError::transport(format!("reading the body of the answer to {context}"), e)
        })? {
            if collected.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(AcmeError::protocol(
                    context,
                    format!("the answer is larger than the {MAX_RESPONSE_BYTES} byte ceiling"),
                ));
            }
            collected.extend_from_slice(&chunk);
        }

        Ok(AcmeResponse {
            status,
            headers,
            body: collected,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_urls_split_into_the_parts_a_request_needs() {
        let url = AcmeUrl::parse("https://acme-v02.api.letsencrypt.org/directory").unwrap();
        assert_eq!(url.host, "acme-v02.api.letsencrypt.org");
        assert_eq!(url.port, 443);
        assert!(url.tls);
        assert_eq!(url.path, "/directory");
        assert_eq!(url.authority(), "acme-v02.api.letsencrypt.org:443");

        // Every URL after the directory comes from the authority itself, so
        // an explicit port and a deep path are the normal case, not an edge
        // one: Pebble answers on 14000 and hands back /acme/order/... URLs.
        let url = AcmeUrl::parse("https://127.0.0.1:14000/acme/order/x-1?q=2").unwrap();
        assert_eq!(url.port, 14000);
        assert_eq!(url.path, "/acme/order/x-1?q=2");

        let url = AcmeUrl::parse("http://[::1]:8080/dir").unwrap();
        assert_eq!(url.host, "::1");
        assert_eq!(url.authority(), "[::1]:8080");
        assert!(!url.tls);
    }

    #[test]
    fn a_url_with_no_path_still_sends_one() {
        assert_eq!(AcmeUrl::parse("https://ca.test").unwrap().path, "/");
        assert_eq!(AcmeUrl::parse("https://ca.test?x=1").unwrap().path, "/?x=1");
    }

    #[test]
    fn anything_that_is_not_an_http_url_is_refused() {
        for bad in [
            "ftp://ca.test/dir",
            "ca.test/dir",
            "https://",
            "https://ca.test:port/dir",
        ] {
            assert!(
                AcmeUrl::parse(bad).is_err(),
                "{bad} should not parse as an ACME URL"
            );
        }
    }

    fn response(headers: &[(&str, &str)]) -> AcmeResponse {
        AcmeResponse {
            status: 200,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    #[test]
    fn headers_are_read_without_regard_to_case() {
        // The one that matters is Replay-Nonce: HTTP/2 lowercases every
        // header name, HTTP/1.1 does not, and a client that only looked for
        // one spelling would work against one authority and not another.
        let response = response(&[("replay-nonce", "abc"), ("location", "https://ca/acct/1")]);
        assert_eq!(response.header("Replay-Nonce"), Some("abc"));
        assert_eq!(response.header("LOCATION"), Some("https://ca/acct/1"));
        assert_eq!(response.header("link"), None);
    }

    #[tokio::test]
    async fn every_resolved_address_is_tried_before_the_authority_is_given_up_on() {
        // The shape of a host with a broken IPv6 route: the first address the
        // resolver returns does not answer, the second does. Only the first
        // used to be tried, so the second never got a chance and every
        // request failed with a timeout that named nothing but the authority.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = listener.local_addr().unwrap();
        let accepted = tokio::spawn(async move { listener.accept().await.is_ok() });
        // Port 1 is tcpmux, which nothing on a modern host listens on, so the
        // connection is refused at once rather than timing out.
        let refusing: SocketAddr = "127.0.0.1:1".parse().unwrap();

        let http = AcmeHttp::new(Duration::from_secs(5));
        let session = http
            .connect_any("ca.test:80", &[refusing, live], false, "")
            .await;
        assert!(
            session.is_ok(),
            "the second address must be tried when the first refuses: {:?}",
            session.err().map(|e| e.to_string())
        );
        assert!(accepted.await.unwrap(), "the live address was connected to");

        // And when nothing accepts, the error names every address tried.
        let error = http
            .connect_any("ca.test:80", &[refusing, refusing], false, "")
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains("127.0.0.1:1"), "{error}");
        assert_eq!(
            error.matches(" | ").count(),
            1,
            "one attempt per address, both reported: {error}"
        );
    }

    #[test]
    fn a_retry_after_in_seconds_is_honoured_and_a_date_is_not_guessed_at() {
        assert_eq!(
            response(&[("retry-after", "5")]).retry_after(),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            response(&[("retry-after", "Fri, 31 Dec 1999 23:59:59 GMT")]).retry_after(),
            None,
            "an unparsed date must fall back to the caller's interval rather than to zero"
        );
    }
}
