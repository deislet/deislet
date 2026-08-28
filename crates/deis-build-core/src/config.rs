use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub version: String,
    pub name: String,
    pub runtime: String,
    pub language: String,
    #[serde(rename = "functionRoot", default = "default_function_root")]
    pub function_root: String,
    #[serde(default)]
    pub routing: Option<RoutingConfig>,
    #[serde(default)]
    pub environment: Option<EnvironmentConfig>,
    #[serde(default)]
    pub services: Option<ServicesConfig>,
    #[serde(default)]
    pub vendors: Option<VendorsConfig>,
    #[serde(default)]
    pub build: Option<BuildConfig>,
    #[serde(default)]
    pub limits: Option<LimitsConfig>,
}

fn default_function_root() -> String {
    "./functions".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(rename = "caseSensitive", default)]
    pub case_sensitive: bool,
    #[serde(rename = "trailingSlash", default)]
    pub trailing_slash: Option<String>,
    #[serde(default)]
    pub routes: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    #[serde(default)]
    pub variables: Option<HashMap<String, String>>,
    #[serde(default)]
    pub secrets: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServicesConfig {
    #[serde(default)]
    pub kv: Option<ServiceConfig>,
    #[serde(default)]
    pub database: Option<ServiceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub enabled: bool,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(rename = "connectionString", default)]
    pub connection_string: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VendorsConfig {
    #[serde(default)]
    pub cloudflare: Option<VendorConfig>,
    #[serde(default)]
    pub deno: Option<VendorConfig>,
    #[serde(default)]
    pub tencent: Option<VendorConfig>,
    #[serde(default)]
    pub deis_runtime: Option<VendorConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VendorConfig {
    pub enabled: bool,
    #[serde(default)]
    pub options: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildConfig {
    #[serde(rename = "outDir", default = "default_out_dir")]
    pub out_dir: String,
    #[serde(default = "default_true")]
    pub minify: bool,
    #[serde(default = "default_true")]
    pub sourcemap: bool,
    #[serde(default)]
    pub target: Option<String>,
}

fn default_out_dir() -> String {
    "./dist".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(default)]
    pub timeout: Option<u32>,
    #[serde(rename = "maxPayloadSize", default)]
    pub max_payload_size: Option<usize>,
    #[serde(rename = "maxConcurrency", default)]
    pub max_concurrency: Option<u32>,
}
