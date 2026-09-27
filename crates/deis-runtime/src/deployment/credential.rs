//! Deciding whether a bundle URL is somewhere this node may present itself.
//!
//! This was once about a credential: the assignment list and the bundle
//! download were HTTP endpoints guarded by a value every machine in the
//! installation held, and the question here was which hosts that value could
//! safely be sent to. A bundle URL comes out of the control plane's own
//! answer, so it can name any host, and sending the installation's shared key
//! to whatever host that is would have put it in a third party's access log.
//!
//! The key is gone and the question is not. [`origin_of`] is what
//! `deployment::fetch` uses to decide whether a bundle URL is on this node's
//! own machine listener. If it is, the download goes over the mutually
//! authenticated connection this node already proved a certificate on; if it
//! is not, it goes out as an ordinary anonymous HTTP request to a third party,
//! which is what a URL naming somewhere else should get. The comparison has to
//! be exact for the same reason it always did - a host that merely looks like
//! the control plane must not be treated as it.

/// `scheme://host:port` in a form two spellings of the same origin share.
///
/// The default port is filled in so `http://host` and `http://host:80` are one
/// origin, and userinfo is dropped so a credential in the URL cannot make the
/// same host look like a different one.
///
/// Shared with [`super::fetch`], which asks the same question about a bundle
/// URL for a different reason - whether it is on the machine listener this
/// node was configured with, and so whether the download speaks mutual TLS.
/// Two spellings of "is this the host we were pointed at" would eventually
/// disagree, and then a node would present a credential on a connection it
/// had decided was somebody else's, or the reverse.
pub(super) fn origin_of(url: &str) -> Option<String> {
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
