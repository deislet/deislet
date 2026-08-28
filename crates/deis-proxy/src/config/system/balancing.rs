//! Upstream selection strategy.

use std::fmt::{self, Display};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, Validatable};

/// How the balancer picks an upstream for a request.
///
/// The strategy is fixed for the lifetime of the process; changing it
/// requires a restart. The upstream list itself can change at runtime.
///
/// Serialized as a string: `round_robin`, `random`, `ip_hash` or
/// `header_hash(<Header-Name>)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum BalancingStrategy {
    /// Cycle through upstreams in order, honouring weights.
    RoundRobin,
    /// Pick a weighted random upstream per request.
    Random,
    /// Hash the value of the named request header; requests carrying the
    /// same value stick to the same upstream.
    HeaderHash(String),
    /// Consistent hashing on the client IP address.
    IpHash,
}

impl FromStr for BalancingStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        match s {
            "round_robin" => return Ok(Self::RoundRobin),
            "random" => return Ok(Self::Random),
            "ip_hash" => return Ok(Self::IpHash),
            _ => {}
        }
        if let Some(rest) = s.strip_prefix("header_hash")
            && let Some(name) = rest
                .trim()
                .strip_prefix('(')
                .and_then(|r| r.strip_suffix(')'))
        {
            let name = name.trim();
            if name.is_empty() {
                return Err("header_hash requires a header name".to_string());
            }
            return Ok(Self::HeaderHash(name.to_string()));
        }
        Err(format!("unsupported balancing strategy: {s}"))
    }
}

impl Display for BalancingStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RoundRobin => write!(f, "round_robin"),
            Self::Random => write!(f, "random"),
            Self::HeaderHash(name) => write!(f, "header_hash({name})"),
            Self::IpHash => write!(f, "ip_hash"),
        }
    }
}

impl TryFrom<String> for BalancingStrategy {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<BalancingStrategy> for String {
    fn from(value: BalancingStrategy) -> Self {
        value.to_string()
    }
}

impl Validatable for BalancingStrategy {
    fn validate(&self) -> Result<(), ConfigError> {
        if let Self::HeaderHash(name) = self
            && name.trim().is_empty()
        {
            return Err(ConfigError::ValidationError {
                message: "header_hash requires a header name".to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_strategies() {
        assert_eq!("round_robin".parse(), Ok(BalancingStrategy::RoundRobin));
        assert_eq!("random".parse(), Ok(BalancingStrategy::Random));
        assert_eq!("ip_hash".parse(), Ok(BalancingStrategy::IpHash));
    }

    #[test]
    fn parses_header_hash_with_loose_spacing() {
        for input in [
            "header_hash(X-User-ID)",
            "header_hash (X-User-ID )",
            "header_hash( X-User-ID)",
        ] {
            assert_eq!(
                input.parse::<BalancingStrategy>(),
                Ok(BalancingStrategy::HeaderHash("X-User-ID".to_string())),
                "{input}"
            );
        }
    }

    #[test]
    fn rejects_malformed_input() {
        for input in [
            "header_hash()",
            "header_hash(   )",
            "header_hash(missing",
            "least_connections",
            "",
        ] {
            assert!(input.parse::<BalancingStrategy>().is_err(), "{input}");
        }
    }

    #[test]
    fn toml_round_trip() {
        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            strategy: BalancingStrategy,
        }
        let parsed: Wrapper = toml::from_str(r#"strategy = "header_hash(Authorization)""#).unwrap();
        assert_eq!(
            parsed.strategy,
            BalancingStrategy::HeaderHash("Authorization".to_string())
        );
        let text = toml::to_string(&parsed).unwrap();
        assert!(text.contains("header_hash(Authorization)"));
    }
}
