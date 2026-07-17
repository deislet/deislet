use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::{system::mime::MIMETypes, ConfigError, Validatable};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CompressionAlgo {
    Gzip,   // High compatibility, widely supported
    Brotli, // High compression ratio, new browser supported
    Zstd,   // High compression ratio and Performance, but not widely supported
}

impl std::str::FromStr for CompressionAlgo {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "gzip" => Ok(CompressionAlgo::Gzip),
            "br" | "brotli" => Ok(CompressionAlgo::Brotli),
            "zstd" => Ok(CompressionAlgo::Zstd),
            _ => Err(format!("Unsupported compression algorithm: {s}")),
        }
    }
}

impl std::fmt::Display for CompressionAlgo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompressionAlgo::Gzip => write!(f, "gzip"),
            CompressionAlgo::Brotli => write!(f, "br"),
            CompressionAlgo::Zstd => write!(f, "zstd"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CompressionLevel {
    FastestSpeed = 1,
    FastSpeed = 2,
    Balancing = 3,
    HighCompression = 6,
    UltraCompression = 9,
}

impl CompressionLevel {
    #[allow(dead_code)]
    fn from_u8(level: u8) -> Result<Self, String> {
        match level {
            1 => Ok(CompressionLevel::FastestSpeed),
            2 => Ok(CompressionLevel::FastSpeed),
            3 => Ok(CompressionLevel::Balancing),
            4..=7 => Ok(CompressionLevel::HighCompression),
            8 | 9 => Ok(CompressionLevel::UltraCompression),
            _ => Err(format!("Invalid compression level: {level}, must be 1-9")),
        }
    }
}

/// Minimum size of the content to compress
/// 
/// The default value is 1024 bytes.
/// The unit can be B, KB, MB, GB, TB.
/// The value can be a decimal number.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MinSize {
    size: f64, // minimum bytes to compress
}

impl MinSize {
    pub fn new(size: f64) -> Self {
        Self { size }
    }

    pub fn from(bytes: &str) -> Result<Self, String> {
        // extract the value and unit from the input
        let (value, unit) = Self::extract_value_and_unit(bytes)?;
        let size = value
            * match unit {
                Some("B") => 1.0,
                Some("KB") => 1024.0,
                Some("MB") => 1024.0 * 1024.0,
                Some("GB") => 1024.0 * 1024.0 * 1024.0,
                Some("TB") => 1024.0 * 1024.0 * 1024.0 * 1024.0,
                Some(_) => return Err("Unsupported unit".to_string()),
                None => 1.0,
            };
        Ok(Self { size })
    }

    fn extract_value_and_unit(input: &str) -> Result<(f64, Option<&str>), String> {
        // define a regex to match the value and unit
        let re = Regex::new(r"^([\d.]+)\s*(\w+)?$").map_err(|_| "Invalid regex")?;

        // use the regex to match the input
        if let Some(captures) = re.captures(input) {
            let value_str = captures.get(1).map_or("", |m| m.as_str());
            let unit_str = captures.get(2).map(|m| m.as_str());

            // convert the value part to f64
            let value: f64 = value_str.parse().map_err(|_| "Invalid number")?;

            // return the extracted value and unit
            Ok((value, unit_str))
        } else {
            Err("Invalid input".to_string())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompressionConfig {
    pub types: Vec<MIMETypes>,   // types of content to compress
    pub algo: CompressionAlgo,   // algorithm to use for compression
    pub level: CompressionLevel, // level of compression
    pub min_size: MinSize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            types: vec![
                MIMETypes::TextPlain,
                MIMETypes::TextHTML,
                MIMETypes::TextCSS,
                MIMETypes::TextJavaScript,
                MIMETypes::TextXML,
                MIMETypes::ApplicationJSON,
                MIMETypes::ApplicationXML,
                MIMETypes::ApplicationJavaScript,
            ],
            algo: CompressionAlgo::Gzip,
            level: CompressionLevel::Balancing,
            min_size: MinSize::new(1024.0),
        }
    }
}

impl Validatable for CompressionConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.types.is_empty() {
            return Err(ConfigError::ValidationError { message: "compression types is empty".to_string() });
        }
        Ok(())
    }
}
