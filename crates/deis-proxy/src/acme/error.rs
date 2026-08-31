//! What an attempt to obtain a certificate can fail with.
//!
//! Each variant exists because it needs a different sentence in the log. An
//! operator reading `renewal failed` learns nothing; reading that the
//! authority answered `urn:ietf:params:acme:error:rateLimited` with its own
//! detail text, or that the challenge for one name was fetched and did not
//! match, tells them which of the four or five things it could be it
//! actually is.

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum AcmeError {
    #[error("{0}")]
    Crypto(String),

    #[error("{0}")]
    Storage(String),

    #[error("{context}: {reason}")]
    Transport { context: String, reason: String },

    #[error("no answer from the certificate authority within {0:?}")]
    Timeout(Duration),

    #[error(
        "the authority answered {status} to {context}: {kind}{detail}. This is the authority's \
         own verdict, not a fault on this side"
    )]
    Authority {
        context: String,
        status: u16,
        /// The `type` member of the RFC 7807 problem document, for example
        /// `urn:ietf:params:acme:error:rateLimited`.
        kind: String,
        detail: String,
    },

    #[error(
        "the authority's answer to {context} is not the ACME this client understands: {reason}"
    )]
    Protocol { context: String, reason: String },

    #[error(
        "the authority could not validate {name}: {detail}. It fetched \
         http://{name}/.well-known/acme-challenge/<token> from wherever that name resolves to, \
         and did not get the answer this instance published - so either the name does not point \
         at this machine, or something in front of it answered port 80 first"
    )]
    ChallengeFailed { name: String, detail: String },

    #[error(
        "the order for {name} was still {status} after {elapsed:?}; it has been abandoned and \
         will be retried from the start"
    )]
    OrderStalled {
        name: String,
        status: String,
        elapsed: Duration,
    },
}

impl AcmeError {
    /// Whether the authority is telling us to stop asking for a while.
    ///
    /// Worth separating because the answer changes what the operator should
    /// do: a rate limit is not a misconfiguration, and retrying harder makes
    /// it worse.
    pub fn is_rate_limit(&self) -> bool {
        matches!(self, AcmeError::Authority { kind, .. } if kind.ends_with(":rateLimited"))
    }

    pub fn transport(context: impl Into<String>, reason: impl std::fmt::Display) -> Self {
        AcmeError::Transport {
            context: context.into(),
            reason: reason.to_string(),
        }
    }

    pub fn protocol(context: impl Into<String>, reason: impl Into<String>) -> Self {
        AcmeError::Protocol {
            context: context.into(),
            reason: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_limit_is_told_apart_from_any_other_refusal() {
        let limited = AcmeError::Authority {
            context: "newOrder".to_string(),
            status: 429,
            kind: "urn:ietf:params:acme:error:rateLimited".to_string(),
            detail: String::new(),
        };
        let malformed = AcmeError::Authority {
            context: "newOrder".to_string(),
            status: 400,
            kind: "urn:ietf:params:acme:error:malformed".to_string(),
            detail: String::new(),
        };
        assert!(limited.is_rate_limit());
        assert!(!malformed.is_rate_limit());
        assert!(!AcmeError::Timeout(Duration::from_secs(1)).is_rate_limit());
    }

    #[test]
    fn a_challenge_failure_names_what_the_authority_actually_did() {
        // The message is the whole value of this variant: the operator has to
        // be able to tell "my DNS is not pointed here" from "my client is
        // broken" without reading the source.
        let message = AcmeError::ChallengeFailed {
            name: "shop.example.com".to_string(),
            detail: "404".to_string(),
        }
        .to_string();
        assert!(message.contains("shop.example.com"));
        assert!(message.contains(".well-known/acme-challenge"));
        assert!(message.contains("port 80"));
    }
}
