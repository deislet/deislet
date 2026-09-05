//! Edge Canon routing/static-assets semantic document.
//!
//! The generic source adapter lowers the project file tree and the existing
//! `_redirects`/`_headers` input into this provider-neutral document once.
//! Provider packagers consume this document; they never reinterpret the
//! author's source rules independently.

use std::collections::HashSet;

use anyhow::Result;
use deis_build_core::ProjectConfig;
use serde::{Deserialize, Serialize};

use crate::canonical_artifact;

pub const FORMAT: &str = "edge-canon.routing-static-assets/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutingDocument {
    pub schema_version: u64,
    pub format: String,
    pub standard_version: String,
    pub matching: MatchingContract,
    pub assets: Vec<AssetRoute>,
    pub redirects: Vec<RedirectRule>,
    pub rewrites: Vec<RewriteRule>,
    pub headers: Vec<HeaderRule>,
    pub functions: Vec<FunctionRoute>,
    pub fallback: Fallback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MatchingContract {
    pub input: String,
    pub case_sensitive: bool,
    pub query: String,
    pub rewrite_passes: u64,
    pub asset_function_precedence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssetRoute {
    pub url_path: String,
    pub file_path: String,
    pub size: u64,
    pub sha256: String,
    pub media_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RedirectRule {
    pub id: String,
    pub source: String,
    pub destination: RedirectDestination,
    pub status: u16,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RedirectDestination {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RewriteRule {
    pub id: String,
    pub source: String,
    pub destination: String,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HeaderRule {
    pub id: String,
    pub source: String,
    pub values: Vec<HeaderValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HeaderValue {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FunctionRoute {
    pub id: String,
    pub source: String,
    pub methods: Vec<String>,
    pub entrypoint_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Fallback {
    NotFound,
    Custom404 {
        #[serde(rename = "filePath")]
        file_path: String,
    },
    Spa {
        #[serde(rename = "filePath")]
        file_path: String,
    },
}

pub fn build(
    standard_version: &str,
    config: &ProjectConfig,
    routes: &[(String, String)],
    assets: &[deis_build_adapter_deislet::StagedAsset],
) -> Result<RoutingDocument> {
    let site = deis_build_adapter_deislet::build_site_config(config, assets)
        .map_err(|_| routing_error("EC_ROUTING_SOURCE_INVALID", "static-site rules are invalid"))?;

    let mut asset_rows = Vec::new();
    for asset in assets
        .iter()
        .filter(|asset| !deis_build_adapter_deislet::is_site_file(&asset.path))
    {
        let file_path = format!("static/{}", asset.path);
        let row = |url_path: String| AssetRoute {
            url_path,
            file_path: file_path.clone(),
            size: asset.bytes.len() as u64,
            sha256: canonical_artifact::sha256(&asset.bytes),
            media_type: asset.content_type.into(),
        };
        asset_rows.push(row(format!("/{}", asset.path)));
        if asset.path == "index.html" {
            asset_rows.push(row("/".into()));
        } else if let Some(directory) = asset.path.strip_suffix("/index.html") {
            asset_rows.push(row(format!("/{directory}")));
        }
    }
    asset_rows.sort_by(|left, right| left.url_path.as_bytes().cmp(right.url_path.as_bytes()));

    let mut redirects = Vec::new();
    let mut rewrites = Vec::new();
    let mut saw_rewrite = false;
    for (index, rule) in site.redirects.iter().enumerate() {
        validate_pattern(&rule.from)?;
        validate_destination(&rule.from, &rule.to)?;
        match rule.status {
            301 | 302 => {
                if saw_rewrite {
                    return Err(routing_error(
                        "EC_ROUTING_SOURCE_UNREPRESENTABLE",
                        "a redirect after a rewrite does not match the standard routing pipeline",
                    ));
                }
                redirects.push(RedirectRule {
                    id: format!("redirect-{}", index + 1),
                    source: rule.from.clone(),
                    destination: RedirectDestination {
                        kind: if rule.to.starts_with("https://") {
                            "https".into()
                        } else {
                            "path".into()
                        },
                        value: rule.to.clone(),
                    },
                    status: rule.status,
                    query: "preserve".into(),
                });
            }
            200 => {
                saw_rewrite = true;
                rewrites.push(RewriteRule {
                    id: format!("rewrite-{}", index + 1),
                    source: rule.from.clone(),
                    destination: rule.to.clone(),
                    query: "preserve".into(),
                });
            }
            _ => {
                return Err(routing_error(
                    "EC_ROUTING_SOURCE_UNREPRESENTABLE",
                    "a source redirect status has no Edge Canon routing equivalent",
                ))
            }
        }
    }

    // Deislet's historical `_headers` input composes every matching block,
    // while the standard fixes first-match semantics. One block is identical;
    // multiple blocks are not lowered until an exact migration algorithm can
    // prove they do not overlap.
    if site.headers.len() > 1 {
        return Err(routing_error(
            "EC_ROUTING_SOURCE_UNREPRESENTABLE",
            "multiple _headers blocks use composition semantics not present in the locked standard",
        ));
    }
    let mut headers = Vec::new();
    for (index, rule) in site.headers.iter().enumerate() {
        validate_pattern(&rule.pattern)?;
        if !rule.remove.is_empty() || rule.set.is_empty() || rule.set.len() > 30 {
            return Err(routing_error(
                "EC_ROUTING_SOURCE_UNREPRESENTABLE",
                "a _headers block uses removal or a size not present in the locked standard",
            ));
        }
        let mut names = HashSet::new();
        let mut values = Vec::new();
        for field in &rule.set {
            if !names.insert(field.name.to_ascii_lowercase()) {
                return Err(routing_error(
                    "EC_ROUTING_SOURCE_UNREPRESENTABLE",
                    "a _headers block repeats a name the locked standard requires to be unique",
                ));
            }
            values.push(HeaderValue {
                name: field.name.clone(),
                value: field.value.clone(),
            });
        }
        headers.push(HeaderRule {
            id: format!("headers-{}", index + 1),
            source: rule.pattern.clone(),
            values,
        });
    }

    let route_plan = deis_build_adapter_deislet::plan_routes(routes);
    let functions = route_plan
        .routes
        .iter()
        .enumerate()
        .map(|(index, route)| {
            validate_pattern(&route.pattern)?;
            Ok(FunctionRoute {
                id: format!("function-{}", index + 1),
                source: route.pattern.clone(),
                methods: vec!["*".into()],
                entrypoint_id: "main".into(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let fallback = match site.not_found {
        deis_bundle::NotFound::None => Fallback::NotFound,
        deis_bundle::NotFound::Page => Fallback::Custom404 {
            file_path: "static/404.html".into(),
        },
        deis_bundle::NotFound::SinglePageApp => Fallback::Spa {
            file_path: "static/index.html".into(),
        },
    };
    let document = RoutingDocument {
        schema_version: 1,
        format: FORMAT.into(),
        standard_version: standard_version.into(),
        matching: MatchingContract {
            input: "normalized-url-pathname".into(),
            case_sensitive: true,
            query: "excluded-from-match".into(),
            rewrite_passes: 1,
            asset_function_precedence: "asset-first".into(),
        },
        assets: asset_rows,
        redirects,
        rewrites,
        headers,
        functions,
        fallback,
    };
    validate(&document)?;
    Ok(document)
}

pub fn parse(bytes: &[u8], expected_standard_version: &str) -> Result<RoutingDocument> {
    let document: RoutingDocument = serde_json::from_slice(bytes).map_err(|_| {
        routing_error(
            "EC_ROUTING_DOCUMENT_INVALID",
            "routing document does not match the supported schema",
        )
    })?;
    validate(&document)?;
    if document.standard_version != expected_standard_version {
        return Err(routing_error(
            "EC_ROUTING_VERSION_UNSUPPORTED",
            "routing document standard version differs from the canonical parent",
        ));
    }
    Ok(document)
}

pub fn validate(document: &RoutingDocument) -> Result<()> {
    if document.schema_version != 1
        || document.format != FORMAT
        || !is_exact_standard(&document.standard_version)
        || document.matching.input != "normalized-url-pathname"
        || !document.matching.case_sensitive
        || document.matching.query != "excluded-from-match"
        || document.matching.rewrite_passes != 1
        || document.matching.asset_function_precedence != "asset-first"
    {
        return Err(routing_error(
            "EC_ROUTING_DOCUMENT_INVALID",
            "routing document identity or matching contract is invalid",
        ));
    }
    let mut ids = HashSet::new();
    let mut urls = HashSet::new();
    for asset in &document.assets {
        validate_url_path(&asset.url_path)?;
        canonical_artifact::validate_portable_path(&asset.file_path).map_err(|_| {
            routing_error(
                "EC_ROUTING_ASSET_PATH_INVALID",
                "routing asset path is invalid",
            )
        })?;
        validate_sha256(&asset.sha256)?;
        if asset.media_type.is_empty()
            || asset.media_type.len() > 256
            || !urls.insert(asset.url_path.as_str())
        {
            return Err(routing_error(
                "EC_ROUTING_ASSET_INVALID",
                "routing asset identity is invalid",
            ));
        }
    }
    for rule in &document.redirects {
        validate_rule_id(&rule.id, &mut ids)?;
        validate_pattern(&rule.source)?;
        validate_destination(&rule.source, &rule.destination.value)?;
        if !matches!(rule.status, 301 | 302)
            || !matches!(rule.query.as_str(), "preserve" | "discard")
            || (rule.destination.kind == "https") != rule.destination.value.starts_with("https://")
            || !matches!(rule.destination.kind.as_str(), "path" | "https")
        {
            return Err(routing_error(
                "EC_ROUTING_REDIRECT_INVALID",
                "routing redirect is invalid",
            ));
        }
    }
    for rule in &document.rewrites {
        validate_rule_id(&rule.id, &mut ids)?;
        validate_pattern(&rule.source)?;
        validate_destination(&rule.source, &rule.destination)?;
        if rule.destination.starts_with("https://")
            || !matches!(rule.query.as_str(), "preserve" | "discard")
        {
            return Err(routing_error(
                "EC_ROUTING_REWRITE_INVALID",
                "routing rewrite is invalid",
            ));
        }
    }
    for rule in &document.headers {
        validate_rule_id(&rule.id, &mut ids)?;
        validate_pattern(&rule.source)?;
        if rule.values.is_empty() || rule.values.len() > 30 {
            return Err(routing_error(
                "EC_ROUTING_HEADER_INVALID",
                "routing header rule is invalid",
            ));
        }
        let mut names = HashSet::new();
        for header in &rule.values {
            validate_header(header)?;
            if !names.insert(header.name.to_ascii_lowercase()) {
                return Err(routing_error(
                    "EC_ROUTING_HEADER_INVALID",
                    "routing header names are not unique",
                ));
            }
        }
    }
    for route in &document.functions {
        validate_rule_id(&route.id, &mut ids)?;
        validate_pattern(&route.source)?;
        if route.methods.is_empty()
            || route.entrypoint_id.is_empty()
            || route
                .methods
                .iter()
                .any(|method| method != "*" && !valid_method(method))
        {
            return Err(routing_error(
                "EC_ROUTING_FUNCTION_INVALID",
                "routing function route is invalid",
            ));
        }
    }
    let fallback_path = match &document.fallback {
        Fallback::NotFound => None,
        Fallback::Custom404 { file_path } | Fallback::Spa { file_path } => Some(file_path),
    };
    if let Some(path) = fallback_path {
        let expected = match document.fallback {
            Fallback::Custom404 { .. } => "/404.html",
            Fallback::Spa { .. } => "/index.html",
            Fallback::NotFound => unreachable!(),
        };
        if !document
            .assets
            .iter()
            .any(|asset| &asset.file_path == path && asset.url_path == expected)
        {
            return Err(routing_error(
                "EC_ROUTING_FALLBACK_INVALID",
                "routing fallback file is not indexed at the required URL",
            ));
        }
    }
    Ok(())
}

pub fn require_target_implementation(provider: &str, document: &RoutingDocument) -> Result<()> {
    if provider == deis_build_adapter_deislet::VENDOR {
        return Ok(());
    }
    let mut missing = Vec::new();
    if !document.redirects.is_empty() {
        missing.push("redirects");
    }
    if !document.rewrites.is_empty() {
        missing.push("rewrites");
    }
    if !document.headers.is_empty() {
        missing.push("static response headers");
    }
    if !matches!(document.fallback, Fallback::NotFound) {
        missing.push("custom fallback");
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(routing_error(
        "EC_ROUTING_TARGET_UNIMPLEMENTED",
        &format!(
            "the {provider} packager has no verified mapping for {}; no provider artifact was produced",
            missing.join(", ")
        ),
    ))
}

pub fn deislet_site_files(document: &RoutingDocument) -> Result<Vec<(&'static str, Vec<u8>)>> {
    validate(document)?;
    let mut files = Vec::new();
    if !document.redirects.is_empty() || !document.rewrites.is_empty() {
        let mut redirects = String::new();
        for rule in &document.redirects {
            if rule.query != "preserve" {
                return Err(routing_error(
                    "EC_ROUTING_TARGET_UNIMPLEMENTED",
                    "the deislet source adapter cannot encode redirect query discard",
                ));
            }
            redirects.push_str(&format!(
                "{} {} {}\n",
                rule.source, rule.destination.value, rule.status
            ));
        }
        for rule in &document.rewrites {
            if rule.query != "preserve" {
                return Err(routing_error(
                    "EC_ROUTING_TARGET_UNIMPLEMENTED",
                    "the deislet source adapter cannot encode rewrite query discard",
                ));
            }
            redirects.push_str(&format!("{} {} 200\n", rule.source, rule.destination));
        }
        files.push(("_redirects", redirects.into_bytes()));
    }
    if !document.headers.is_empty() {
        let mut headers = String::new();
        for rule in &document.headers {
            headers.push_str(&rule.source);
            headers.push('\n');
            for value in &rule.values {
                headers.push_str(&format!("  {}: {}\n", value.name, value.value));
            }
        }
        files.push(("_headers", headers.into_bytes()));
    }
    Ok(files)
}

fn validate_pattern(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 500
        || !value.starts_with('/')
        || value.bytes().any(|byte| !(0x20..=0x7e).contains(&byte))
        || value.contains(['?', '#', '\\'])
    {
        return Err(routing_error(
            "EC_ROUTING_PATTERN_INVALID",
            "routing pattern is invalid",
        ));
    }
    let segments = if value == "/" {
        Vec::new()
    } else {
        value[1..].split('/').collect::<Vec<_>>()
    };
    let mut parameters = HashSet::new();
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            return Err(routing_error(
                "EC_ROUTING_PATTERN_INVALID",
                "routing pattern is invalid",
            ));
        }
        if *segment == "*" {
            if index + 1 != segments.len() {
                return Err(routing_error(
                    "EC_ROUTING_PATTERN_INVALID",
                    "routing wildcard is not final",
                ));
            }
            parameters.insert("splat");
        } else if let Some(name) = segment.strip_prefix(':') {
            if !valid_parameter(name) || !parameters.insert(name) {
                return Err(routing_error(
                    "EC_ROUTING_PATTERN_INVALID",
                    "routing parameter is invalid",
                ));
            }
        } else if segment.contains([':', '*']) {
            return Err(routing_error(
                "EC_ROUTING_PATTERN_INVALID",
                "routing literal is invalid",
            ));
        }
    }
    Ok(())
}

fn validate_destination(source: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 500
        || value.bytes().any(|byte| !(0x20..=0x7e).contains(&byte))
        || value.contains(['?', '#', '\\'])
    {
        return Err(routing_error(
            "EC_ROUTING_DESTINATION_INVALID",
            "routing destination is invalid",
        ));
    }
    if let Some(rest) = value.strip_prefix("https://") {
        let authority = rest.split('/').next().unwrap_or_default();
        if authority.is_empty() || authority.contains(['@', ':']) {
            return Err(routing_error(
                "EC_ROUTING_DESTINATION_INVALID",
                "external redirect destination is invalid",
            ));
        }
    } else if !value.starts_with('/') || value.starts_with("//") {
        return Err(routing_error(
            "EC_ROUTING_DESTINATION_INVALID",
            "routing destination is invalid",
        ));
    }
    let source_names = pattern_names(source);
    for name in template_names(value) {
        if !source_names.contains(name) {
            return Err(routing_error(
                "EC_ROUTING_DESTINATION_INVALID",
                "routing destination references an unknown parameter",
            ));
        }
    }
    Ok(())
}

fn pattern_names(value: &str) -> HashSet<&str> {
    value
        .split('/')
        .filter_map(|segment| {
            if segment == "*" {
                Some("splat")
            } else {
                segment.strip_prefix(':')
            }
        })
        .collect()
}

fn template_names(value: &str) -> Vec<&str> {
    value
        .split('/')
        .filter_map(|segment| segment.strip_prefix(':'))
        .collect()
}

fn validate_url_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 1024
        || !value.starts_with('/')
        || value.contains(['?', '#', '\\'])
        || value.chars().any(char::is_control)
    {
        return Err(routing_error(
            "EC_ROUTING_ASSET_PATH_INVALID",
            "routing asset URL path is invalid",
        ));
    }
    Ok(())
}

fn validate_rule_id<'a>(value: &'a str, ids: &mut HashSet<&'a str>) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value.as_bytes()[0].is_ascii_lowercase()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !ids.insert(value)
    {
        return Err(routing_error(
            "EC_ROUTING_DOCUMENT_INVALID",
            "routing rule identifier is invalid or duplicated",
        ));
    }
    Ok(())
}

fn validate_header(header: &HeaderValue) -> Result<()> {
    let name = header.name.as_bytes();
    let value = header.value.as_bytes();
    let forbidden = [
        "connection",
        "content-encoding",
        "content-length",
        "etag",
        "keep-alive",
        "location",
        "proxy-authenticate",
        "proxy-authorization",
        "set-cookie",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];
    if name.is_empty()
        || name.len() > 100
        || !name[0].is_ascii_alphabetic()
        || !name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        || value.is_empty()
        || value.len() > 1000
        || !value.iter().all(|byte| (0x20..=0x7e).contains(byte))
        || forbidden
            .iter()
            .any(|item| header.name.eq_ignore_ascii_case(item))
        || header.name.to_ascii_lowercase().starts_with("cf-")
        || header.name.to_ascii_lowercase().starts_with("eo-")
        || header.name.to_ascii_lowercase().starts_with("x-deis-")
    {
        return Err(routing_error(
            "EC_ROUTING_HEADER_INVALID",
            "routing header is invalid or reserved",
        ));
    }
    Ok(())
}

fn valid_parameter(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_method(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value.as_bytes()[0].is_ascii_uppercase()
        && value.bytes().all(|byte| {
            byte.is_ascii_uppercase() || byte.is_ascii_digit() || b"!#$%&'*+-.^_`|~".contains(&byte)
        })
}

fn is_exact_standard(value: &str) -> bool {
    value
        .strip_prefix("edge-canon.next@")
        .is_some_and(|commit| {
            commit.len() == 40
                && commit
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(routing_error(
            "EC_ROUTING_DOCUMENT_INVALID",
            "routing digest is invalid",
        ));
    }
    Ok(())
}

fn routing_error(code: &str, message: &str) -> anyhow::Error {
    anyhow::anyhow!("{code}: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(not_found: Option<&str>) -> ProjectConfig {
        serde_json::from_value(serde_json::json!({
            "version": "1.0.0",
            "name": "site",
            "runtime": "standard-v1",
            "language": "typescript",
            "assets": {"dir": "./public", "notFound": not_found}
        }))
        .expect("config")
    }

    fn asset(path: &str, bytes: &[u8]) -> deis_build_adapter_deislet::StagedAsset {
        deis_build_adapter_deislet::StagedAsset {
            path: path.into(),
            bytes: bytes.to_vec(),
            sha256: canonical_artifact::sha256(bytes),
            content_type: deis_bundle::guess_content_type(path),
        }
    }

    #[test]
    fn document_binds_assets_routes_rules_and_fallback() {
        let assets = vec![
            asset("index.html", b"index"),
            asset("_redirects", b"/old/:id /new/:id 301\n"),
            asset("_headers", b"/*\n  X-Frame-Options: DENY\n"),
        ];
        let document = build(
            "edge-canon.next@f8eabb6619a50aab02fd44647c3bc7d64d817d24",
            &config(Some("single-page-app")),
            &[("/api/:id".into(), "../src/api/[id].ts".into())],
            &assets,
        )
        .expect("routing document");
        assert_eq!(
            document.assets.len(),
            2,
            "index.html also owns the root URL"
        );
        assert_eq!(document.redirects.len(), 1);
        assert_eq!(document.headers.len(), 1);
        assert_eq!(document.functions.len(), 1);
        assert!(matches!(document.fallback, Fallback::Spa { .. }));
    }

    #[test]
    fn source_semantics_that_cannot_be_lowered_fail_closed() {
        for controls in [
            vec![asset("_redirects", b"/old /new 307\n")],
            vec![asset(
                "_headers",
                b"/*\n  X-One: one\n/path\n  X-Two: two\n",
            )],
        ] {
            let error = build(
                "edge-canon.next@f8eabb6619a50aab02fd44647c3bc7d64d817d24",
                &config(None),
                &[],
                &controls,
            )
            .expect_err("unrepresentable source")
            .to_string();
            assert!(
                error.contains("EC_ROUTING_SOURCE_UNREPRESENTABLE"),
                "{error}"
            );
        }
    }
}
