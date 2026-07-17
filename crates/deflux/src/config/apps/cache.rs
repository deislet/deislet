use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::config::{util::CacheKeys, ConfigError, Validatable};

// default_cache_key: CacheKeys::from_str("$scheme$host$request_uri").unwrap(),

// only one of these can be set
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Condition {
    Always,
    Exist(String),
    NotExist(String),
    ExistAndEq(String, String),
    ExistAndNotEq(String, String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CacheConfigEntry(Condition, CacheKeys);

// Vec<(Condition, CacheKeys)>
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CacheConfig(Vec<CacheConfigEntry>);

impl Validatable for CacheConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        for entry in &self.0 {
            entry.validate()?;
        }
        Ok(())
    }
}

impl Validatable for CacheConfigEntry {
    fn validate(&self) -> Result<(), ConfigError> {
        self.0.validate()?;
        self.1.validate()?;
        Ok(())
    }
}

impl Validatable for Condition {
    fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::Always => Ok(()),
            Self::Exist(key) | Self::NotExist(key) => {
                if key.is_empty() {
                    Err(ConfigError::ValidationError {
                        message: "Cache condition key cannot be empty".to_string(),
                    })
                } else {
                    Ok(())
                }
            }
            Self::ExistAndEq(key, value) | Self::ExistAndNotEq(key, value) => {
                if key.is_empty() {
                    Err(ConfigError::ValidationError {
                        message: "Cache condition key cannot be empty".to_string(),
                    })
                } else if value.is_empty() {
                    Err(ConfigError::ValidationError {
                        message: "Cache condition value cannot be empty".to_string(),
                    })
                } else {
                    Ok(())
                }
            }
        }
    }
}

impl Validatable for CacheKeys {
    fn validate(&self) -> Result<(), ConfigError> {
        // TODO: Implement validation
        Ok(())
    }
}

impl Condition {
    #[allow(dead_code)]
    fn from_str(s: &str) -> Result<Self, ConfigError> {
        if s == "always" {
            Ok(Self::Always)
        } else if s.starts_with("exists:") {
            let key = s.split_at(7).1;
            Ok(Self::Exist(key.to_owned()))
        } else if s.starts_with("not_exists:") {
            let key = s.split_at(11).1;
            Ok(Self::NotExist(key.to_owned()))
        } else if s.starts_with("exists_and_eq:") {
            let key_value = s.split_at(14).1;
            match key_value.find(':') {
                Some(colon_pos) => {
                    let (key, value_with_colon) = key_value.split_at(colon_pos);
                    let value = &value_with_colon[1..]; // Skip the colon
                    Ok(Self::ExistAndEq(key.to_owned(), value.to_owned()))
                }
                None => Err(ConfigError::ValidationError {
                    message: format!("Invalid exists_and_eq condition format: missing ':' in '{}'.", s),
                })
            }
        } else if s.starts_with("exists_and_not_eq:") {
            let key_value = s.split_at(18).1;
            match key_value.find(':') {
                Some(colon_pos) => {
                    let (key, value_with_colon) = key_value.split_at(colon_pos);
                    let value = &value_with_colon[1..]; // Skip the colon
                    Ok(Self::ExistAndNotEq(key.to_owned(), value.to_owned()))
                }
                None => Err(ConfigError::ValidationError {
                    message: format!("Invalid exists_and_not_eq condition format: missing ':' in '{}'.", s),
                })
            }
        } else {
            Err(ConfigError::ValidationError {
                message: format!("Invalid cache condition: '{}'. Expected one of: always, exists:, not_exists:, exists_and_eq:, exists_and_not_eq:", s),
            })
        }
    }
}

impl CacheConfigEntry {
    #[allow(dead_code)]
    fn from_str_pair(c: &str, k: &str) -> Result<Self, ConfigError> {
        let condition = Condition::from_str(c)?;
        let cache_key = CacheKeys::from_str(k).map_err(|e| ConfigError::CacheKeyParseError(e))?;
        Ok(Self(condition, cache_key))
    }
}
