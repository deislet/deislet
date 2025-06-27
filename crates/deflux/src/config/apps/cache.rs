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
        // TODO: Implement validation
        Ok(())
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
    fn from_str(s: &str) -> Self {
        if s == "always" {
            Self::Always
        } else if s.starts_with("exists:") {
            let key = s.split_at(7).1;
            Self::Exist(key.to_owned())
        } else if s.starts_with("not_exists:") {
            let key = s.split_at(11).1;
            Self::NotExist(key.to_owned())
        } else if s.starts_with("exists_and_eq:") {
            let key_value = s.split_at(14).1;
            let (key, value) = key_value.split_at(key_value.find(':').unwrap());
            Self::ExistAndEq(key.to_owned(), value.to_owned())
        } else if s.starts_with("exists_and_not_eq:") {
            let key_value = s.split_at(18).1;
            let (key, value) = key_value.split_at(key_value.find(':').unwrap());
            Self::ExistAndNotEq(key.to_owned(), value.to_owned())
        } else {
            panic!("Invalid condition: {}", s);
        }
    }
}

impl CacheConfigEntry {
    #[allow(dead_code)]
    fn from_str_pair(c: &str, k: &str) -> Result<Self, ConfigError> {
        let condition = Condition::from_str(c);
        let cache_key = CacheKeys::from_str(k).map_err(|e| ConfigError::CacheKeyParseError(e))?;
        Ok(Self(condition, cache_key))
    }
}
