use std::str::FromStr;

use serde::{Deserialize, Serialize};
use regex::Regex;

/// Helper struct to parse size strings like "16GB", "1024KB", etc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SizeValue {
    bytes: u64,
}

impl SizeValue {
    pub fn new(bytes: u64) -> Self {
        Self { bytes }
    }

    /// Parse size string into bytes
    /// Supports: B, KB, MB, GB, TB
    pub fn from_str(size_str: &str) -> Result<Self, String> {
        let (value, unit) = Self::extract_value_and_unit(size_str)?;
        let bytes = (value
            * match unit {
                Some("B") => 1.0,
                Some("KB") => 1024.0,
                Some("MB") => 1024.0 * 1024.0,
                Some("GB") => 1024.0 * 1024.0 * 1024.0,
                Some("TB") => 1024.0 * 1024.0 * 1024.0 * 1024.0,
                Some(_) => return Err(format!("Unsupported unit in: {size_str}")),
                None => 1.0,
            }) as u64;
        Ok(Self { bytes })
    }

    fn extract_value_and_unit(input: &str) -> Result<(f64, Option<&str>), String> {
        let re = Regex::new(r"^([\d.]+)\s*([A-Za-z]+)?$").map_err(|_| "Invalid regex")?;

        if let Some(captures) = re.captures(input) {
            let value_str = captures.get(1).map_or("", |m| m.as_str());
            let unit_str = captures.get(2).map(|m| m.as_str());

            let value: f64 = value_str.parse().map_err(|_| "Invalid number")?;
            Ok((value, unit_str))
        } else {
            Err(format!("Invalid input: {input}"))
        }
    }

    pub fn as_bytes(&self) -> u64 {
        self.bytes
    }
}

/// Cache key types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CacheK {
    Scheme,
    Host,
    RequestUri,
    QueryString,
    Method,
    Protocol,
    Header(String),  // header(header_name)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CacheKeys(pub Vec<CacheK>);

impl FromStr for CacheKeys {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Ok(CacheKeys(vec![]));
        }

        let parts: Vec<&str> = s.split('$').filter(|part| !part.is_empty()).collect();
        let mut result = Vec::new();
        for part in parts {
            match part {
                "scheme" => result.push(CacheK::Scheme),
                "host" => result.push(CacheK::Host),
                "request_uri" => result.push(CacheK::RequestUri),
                "query_string" => result.push(CacheK::QueryString),
                "method" => result.push(CacheK::Method),
                "protocol" => result.push(CacheK::Protocol),
                _ => {
                    // Use regex to parse header(header_name) and cookie(cookie_name)
                    let header_re = Regex::new(r"^header\s*\(([^)]+)\)$")
                        .map_err(|_| "Invalid header regex pattern".to_string())?;
                    
                    if let Some(captures) = header_re.captures(part) {
                        if let Some(header_name) = captures.get(1) {
                            let header_name = header_name.as_str().trim();
                            if header_name.is_empty() {
                                return Err("Header name cannot be empty".to_string());
                            }
                            result.push(CacheK::Header(header_name.to_string()));
                            continue;
                        }
                    }

                    // If no regex matches, it's an invalid cache key part
                    return Err(format!("Invalid cache key part: {}", part));
                }
            }
        }
        Ok(CacheKeys(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cache_key_basic() {
        // Test basic cache key parsing
        let result = CacheKeys::from_str("$scheme$host$request_uri").unwrap();
        assert_eq!(
            result,
            CacheKeys(vec![
                CacheK::Scheme,
                CacheK::Host,
                CacheK::RequestUri
            ])
        );
    }

    #[test]
    fn test_parse_cache_key_all_types() {
        // Test all cache key types
        let result = CacheKeys::from_str("$scheme$host$request_uri$query_string$method$protocol").unwrap();
        assert_eq!(
            result,
            CacheKeys(vec![
                CacheK::Scheme,
                CacheK::Host,
                CacheK::RequestUri,
                CacheK::QueryString,
                CacheK::Method,
                CacheK::Protocol
            ])
        );
    }

    #[test]
    fn test_parse_cache_key_with_header() {
        // Test header parsing
        let result = CacheKeys::from_str("$scheme$host$header(X-User-ID)").unwrap();
        assert_eq!(
            result,
            CacheKeys(vec![
                CacheK::Scheme,
                CacheK::Host,
                CacheK::Header("X-User-ID".to_string())
            ])
        );

        // Test header with spaces
        let result = CacheKeys::from_str("$scheme$header( Authorization )").unwrap();
        assert_eq!(
            result,
            CacheKeys(vec![
                CacheK::Scheme,
                CacheK::Header("Authorization".to_string())
            ])
        );
    }

    #[test]
    fn test_parse_cache_key_complex() {
        // Test complex cache key with multiple types
        let result = CacheKeys::from_str("$scheme$host$request_uri$header(X-User-ID)$method").unwrap();
        assert_eq!(
            result,
            CacheKeys(vec![
                CacheK::Scheme,
                CacheK::Host,
                CacheK::RequestUri,
                CacheK::Header("X-User-ID".to_string()),
                CacheK::Method
            ])
        );
    }

    #[test]
    fn test_parse_cache_key_errors() {
        // Test empty header name
        let result = CacheKeys::from_str("$scheme$header()");
        assert!(result.is_err());

        let result = CacheKeys::from_str("$scheme$header(   )");
        assert!(result.is_err());

        // Test malformed header
        let result = CacheKeys::from_str("$scheme$header(missing_paren");
        assert!(result.is_err());

        // Test invalid cache key part
        let result = CacheKeys::from_str("$scheme$invalid_part");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_cache_key_edge_cases() {
        // Test empty string
        let result = CacheKeys::from_str("").unwrap();
        assert_eq!(result, CacheKeys(vec![]));

        // Test single $ at the beginning
        let result = CacheKeys::from_str("$scheme").unwrap();
        assert_eq!(result, CacheKeys(vec![CacheK::Scheme]));

        // Test multiple $ separators (should be handled correctly)
        let result = CacheKeys::from_str("$$scheme$$host$$").unwrap();
        assert_eq!(
            result,
            CacheKeys(vec![CacheK::Scheme, CacheK::Host])
        );
    }

    /// Demonstration of parse_cache_key functionality
    #[test]
    fn test_parse_cache_key_demo() {
        println!("\n🎯 Cache Key Parsing 演示:");
        
        let examples = vec![
            "$scheme$host$request_uri",
            "$scheme$host$request_uri$header(X-User-ID)",
            "$scheme$host$cookie(session_id)$header(Authorization)",
            "$method$host$request_uri$query_string$header(Content-Type)$cookie(user_token)",
        ];
        
        for example in examples {
            match CacheKeys::from_str(example) {
                Ok(keys) => {
                    println!("✅ 输入: {}", example);
                    println!("   解析结果: {:?}", keys);
                },
                Err(e) => {
                    println!("❌ 输入: {}", example);
                    println!("   错误: {}", e);
                }
            }
            println!();
        }
    }
}