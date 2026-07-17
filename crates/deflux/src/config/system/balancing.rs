use std::fmt::{self, Display};
use std::str::FromStr;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, Validatable};

/// Load balancing strategy
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum BalancingStrategy {
    /// Round robin
    RoundRobin,
    /// Least connections
    LeastConnections,
    /// Maximum power reserve
    MaxPowerReserve,
    /// The tasks are distributed according to the HTTP header field
    HeaderHash(String),
}

impl FromStr for BalancingStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "max_power_reserve" => Ok(BalancingStrategy::MaxPowerReserve),
            _ => {
                // Use regex to match header_hash(header_name) pattern
                let re = Regex::new(r"^header_hash\s*?\(([^)]+)\)$")
                    .map_err(|_| "Invalid regex pattern".to_string())?;
                
                if let Some(captures) = re.captures(s)
                    && let Some(header_name) = captures.get(1)
                {
                    let header_name = header_name.as_str().trim();
                    if header_name.is_empty() {
                        return Err("Header name cannot be empty".to_string());
                    }
                    return Ok(BalancingStrategy::HeaderHash(header_name.to_string()));
                }
                
                // if the string is not a valid balancing strategy, return an error
                Err(format!("Unsupported balancing strategy: {s}"))
            }
        }
    }
}

impl Display for BalancingStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BalancingStrategy::RoundRobin => write!(f, "round_robin"),
            BalancingStrategy::LeastConnections => write!(f, "least_connections"),
            BalancingStrategy::MaxPowerReserve => write!(f, "max_power_reserve"),
            BalancingStrategy::HeaderHash(header_name) => write!(f, "header_hash({})", header_name),
        }
    }
}

impl Validatable for BalancingStrategy {
    fn validate(&self) -> Result<(), ConfigError> {
        if let BalancingStrategy::HeaderHash(header_name) = self
            && header_name.is_empty()
        {
            return Err(ConfigError::ValidationError { message: "header name is empty".to_string() });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_hash_parsing() {
        // Test successful parsing
        let result = BalancingStrategy::from_str("header_hash(X-User-ID)");
        assert_eq!(result, Ok(BalancingStrategy::HeaderHash("X-User-ID".to_string())));

        let result = BalancingStrategy::from_str("header_hash (Authorization )");
        assert_eq!(result, Ok(BalancingStrategy::HeaderHash("Authorization".to_string())));

        // Test with spaces
        let result = BalancingStrategy::from_str("header_hash( Content-Type )");
        assert_eq!(result, Ok(BalancingStrategy::HeaderHash("Content-Type".to_string())));

        // Test Display formatting
        let strategy = BalancingStrategy::HeaderHash("X-User-ID".to_string());
        assert_eq!(format!("{}", strategy), "header_hash(X-User-ID)");
    }

    #[test]
    fn test_header_hash_parsing_errors() {
        // Test empty header name
        let result = BalancingStrategy::from_str("header_hash()");
        assert!(result.is_err());

        let result = BalancingStrategy::from_str("header_hash(   )");
        assert!(result.is_err());

        // Test malformed input
        let result = BalancingStrategy::from_str("header_hash(missing_paren");
        assert!(result.is_err());

        let result = BalancingStrategy::from_str("header_hashmissing_parens");
        assert!(result.is_err());
    }

    #[test]
    fn test_basic_strategies() {
        assert_eq!(BalancingStrategy::from_str("max_power_reserve"), Ok(BalancingStrategy::MaxPowerReserve));
    }
}
