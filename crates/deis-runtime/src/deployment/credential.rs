//! The credential this node presents to the control plane over HTTP.
//!
//! The control plane's two node-facing endpoints - the assignment list and the
//! bundle download - require one of its `node_preshared_keys`, because the
//! assignment list carries every environment's decrypted secrets. It is the
//! same key this node's operator already puts in `[control] secret`; a node
//! has one credential and not two.
//!
//! The key is only sent to the origin the node was configured to talk to. A
//! bundle URL comes from the control plane's answer, so it can name any host;
//! sending the fleet's node key to whatever host that is would put it in a
//! third party's access log. When the URL points somewhere else the download
//! goes out anonymous and says so, which turns into a plain HTTP 401 rather
//! than a leak.

use tracing::warn;

/// A node key plus the single origin it may be sent to.
#[derive(Clone)]
pub struct NodeCredential {
    header_value: String,
    /// Normalised `scheme://host:port` of the configured control plane, or
    /// `None` when no endpoint was configured, in which case nothing is ever
    /// considered same-origin.
    origin: Option<String>,
}

/// Kept out of `{:?}` deliberately: a deployment error that prints its
/// context must not print the fleet's node key.
impl std::fmt::Debug for NodeCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeCredential")
            .field("secret", &"<redacted>")
            .field("origin", &self.origin)
            .finish()
    }
}

impl NodeCredential {
    /// `control_endpoint` is the base URL the node was configured with; only
    /// requests to that origin carry the key.
    pub fn new(secret: &str, control_endpoint: Option<&str>) -> Self {
        Self {
            header_value: format!("Bearer {secret}"),
            origin: control_endpoint.and_then(origin_of),
        }
    }

    /// The `Authorization` value, ready to send.
    pub fn header_value(&self) -> &str {
        &self.header_value
    }

    /// Whether `url` is the control plane this node was pointed at.
    pub fn covers(&self, url: &str) -> bool {
        match (&self.origin, origin_of(url)) {
            (Some(configured), Some(target)) => *configured == target,
            _ => false,
        }
    }

    /// The `Authorization` value for `url`, or `None` with a warning naming
    /// the consequence when the URL is not the configured control plane.
    pub fn header_value_for(&self, url: &str) -> Option<&str> {
        if self.covers(url) {
            return Some(&self.header_value);
        }
        warn!(
            url,
            control_origin = self.origin.as_deref().unwrap_or("<none>"),
            "A bundle download points somewhere other than the configured control plane, so this \
             node's key is withheld from it; if that host requires the key the download fails with \
             HTTP 401 and the currently running version stays in place"
        );
        None
    }
}

/// `scheme://host:port` in a form two spellings of the same origin share.
///
/// The default port is filled in so `http://host` and `http://host:80` are one
/// origin, and userinfo is dropped so a credential in the URL cannot make the
/// same host look like a different one.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|a| !a.is_empty())?;
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let authority = authority.to_ascii_lowercase();

    let has_port = match authority.rfind(':') {
        // An IPv6 literal's own colons do not count; only one after the
        // closing bracket is a port.
        Some(index) => {
            authority[index..]
                .chars()
                .skip(1)
                .all(|c| c.is_ascii_digit())
                && authority[index..].len() > 1
                && !authority[index..].contains(']')
        }
        None => false,
    };
    if has_port {
        return Some(format!("{scheme}://{authority}"));
    }

    let default_port = match scheme.as_str() {
        "http" => "80",
        "https" => "443",
        _ => return Some(format!("{scheme}://{authority}")),
    };
    Some(format!("{scheme}://{authority}:{default_port}"))
}

#[cfg(test)]
#[path = "credential_test.rs"]
mod credential_test;
