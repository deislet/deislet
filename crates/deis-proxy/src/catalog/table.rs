//! The compiled routing table: what a request is matched against.
//!
//! Built once per accepted document and swapped in whole (see
//! [`super`]), so the request path does no parsing, no allocation of the
//! table and no locking - it clones one `Arc` and reads.
//!
//! Matching goes from the most specific host to the least, and only then
//! looks at path prefixes within that host:
//!
//! 1. an exact host (`shop.example.com`);
//! 2. a wildcard host (`*.example.com`), longest suffix first, so
//!    `*.eu.example.com` beats `*.example.com`;
//! 3. an automatic preview address, which is one exact host per environment
//!    (see [`PreviewZone`]);
//! 4. a route that names no host at all, which the catalog means as "any
//!    domain" and is therefore the least specific thing in the table.
//!
//! Within one host, the candidates are ordered by priority (highest first)
//! and then by prefix length (longest first), so `/api/v2` beats `/api`
//! beats `/` at equal priority and an operator can still override that with
//! `priority`. The first candidate whose prefix matches wins.

use std::collections::{BTreeSet, HashMap, HashSet};

use deis_wire::conventions::DEFAULT_ENVIRONMENT;
use tracing::warn;

use crate::catalog::wire::RoutingTableDocument;
use crate::config::routes::normalize_host;

/// Longest a single DNS label may be.
const MAX_LABEL_LEN: usize = 63;

/// Separator between the environment name and the app id in an automatic
/// preview address.
///
/// It is inside one DNS label, not a dot, and that is the whole point: see
/// [`PreviewZone`].
pub const PREVIEW_SEPARATOR: &str = "--";

/// Where an automatic per-environment address lives, when the operator
/// asked for one.
///
/// The shape is `<env>--<app>.<zone>`, plus the shorter `<app>.<zone>` for
/// the default environment. Two labels (`<env>.<app>.<zone>`) reads better
/// and was the first choice; it was rejected because of certificates. A
/// wildcard certificate matches exactly one label, so `*.<zone>` covers
/// `pr-42--shop.<zone>` but not `pr-42.shop.<zone>` - that would need a
/// separate `*.shop.<zone>` certificate for every application, issued at the
/// moment someone creates an app. On a private platform with one wildcard
/// certificate for its preview zone, the one-label form is the difference
/// between preview URLs working out of the box and working after a
/// per-application certificate dance.
///
/// Leaving `preview_zone` unset turns the whole mechanism off: no address is
/// synthesised, and only routes an operator actually created exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewZone(String);

impl PreviewZone {
    /// Normalise a configured zone. `None` when the value is not usable as
    /// a domain suffix, which the caller reports.
    pub fn new(zone: &str) -> Option<Self> {
        let zone = normalize_host(zone.trim().trim_matches('.'));
        if zone.is_empty() || zone.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return None;
        }
        Some(Self(zone))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The address of one environment, or `None` when the app id or the
    /// environment name cannot be a DNS label.
    ///
    /// The catalog allows `_` in both (they are storage tenant halves before
    /// they are hostnames); a hostname does not, so such an environment
    /// simply has no automatic address and keeps whatever explicit route it
    /// was given.
    pub fn address(&self, app_id: &str, environment: &str) -> Option<String> {
        let label = if environment == DEFAULT_ENVIRONMENT {
            app_id.to_string()
        } else {
            format!("{environment}{PREVIEW_SEPARATOR}{app_id}")
        };
        if !is_dns_label(&label) {
            return None;
        }
        Some(format!("{label}.{}", self.0))
    }
}

fn is_dns_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_LEN
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// One environment of one application: the answer a match produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub app_id: String,
    pub environment: String,
}

/// One route, compiled.
#[derive(Debug, Clone)]
struct CompiledRoute {
    target: Target,
    path_prefix: String,
    priority: i64,
}

impl CompiledRoute {
    /// Whether this route's prefix covers `path`.
    ///
    /// A prefix matches on segment boundaries only: `/api` covers `/api`,
    /// `/api/` and `/api/v1`, and does not cover `/apiary`. Without that,
    /// binding `/api` to one application would quietly capture every path
    /// that merely starts with those letters.
    fn covers(&self, path: &str) -> bool {
        let prefix = self.path_prefix.trim_end_matches('/');
        if prefix.is_empty() {
            return true;
        }
        let Some(rest) = path.strip_prefix(prefix) else {
            return false;
        };
        rest.is_empty() || rest.starts_with('/')
    }
}

/// How a request found its application. Carried into the access path so a
/// log line can say which of the two mechanisms answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchSource {
    /// A route an operator created in the catalog.
    Route,
    /// The automatic address of an environment.
    Preview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub target: Target,
    pub source: MatchSource,
}

/// Immutable snapshot of the catalog's routing table.
#[derive(Default)]
pub struct CatalogRoutes {
    /// Version the control plane gave this table. Empty in the table an
    /// instance starts with, which is how "we have never heard from the
    /// control plane" is told apart from "we have an empty table".
    version: String,
    generated_at: i64,
    by_host: HashMap<String, Vec<CompiledRoute>>,
    /// `(".example.com", route)`, longest suffix first.
    wildcards: Vec<(String, CompiledRoute)>,
    any_host: Vec<CompiledRoute>,
    preview: HashMap<String, Target>,
    /// App ids that have a default environment, for the `x-deis-app-id`
    /// header path, which names an application and not an environment.
    apps_with_default_env: HashSet<String>,
    route_count: usize,
    environment_count: usize,
}

impl CatalogRoutes {
    /// Compile a validated document. `zone` turns automatic preview
    /// addresses on; `None` is off.
    ///
    /// Every entry in the document is used: an entry that could not be
    /// applied would make the table partial, and the caller has already
    /// refused any document that contains one
    /// ([`RoutingTableDocument::validate`]). The one thing skipped here is a
    /// *synthesised* preview address whose name cannot be a hostname, which
    /// is not part of the document at all.
    pub fn build(document: &RoutingTableDocument, zone: Option<&PreviewZone>) -> Self {
        let mut table = Self {
            version: document.version.clone(),
            generated_at: document.generated_at,
            route_count: document.routes.len(),
            environment_count: document.environments.len(),
            ..Default::default()
        };

        for entry in &document.routes {
            let route = CompiledRoute {
                target: Target {
                    app_id: entry.app_id.clone(),
                    environment: entry.environment.clone(),
                },
                path_prefix: entry.path_prefix.clone(),
                priority: entry.priority,
            };
            match entry.host.as_deref() {
                None => table.any_host.push(route),
                Some(host) => {
                    let host = normalize_host(host);
                    match host.strip_prefix("*.") {
                        Some(suffix) => table.wildcards.push((format!(".{suffix}"), route)),
                        None => table.by_host.entry(host).or_default().push(route),
                    }
                }
            }
        }

        for candidates in table.by_host.values_mut() {
            sort_candidates(candidates);
        }
        sort_candidates(&mut table.any_host);
        // Longest suffix first, then the usual order inside one suffix.
        table.wildcards.sort_by(|(a_suffix, a), (b_suffix, b)| {
            b_suffix
                .len()
                .cmp(&a_suffix.len())
                .then_with(|| a_suffix.cmp(b_suffix))
                .then_with(|| candidate_order(a, b))
        });

        for env in &document.environments {
            if env.environment == DEFAULT_ENVIRONMENT {
                table.apps_with_default_env.insert(env.app_id.clone());
            }
            let Some(zone) = zone else { continue };
            let target = Target {
                app_id: env.app_id.clone(),
                environment: env.environment.clone(),
            };
            // The default environment answers on both `<app>.<zone>` and
            // `production--<app>.<zone>`, so one form works for every
            // environment and the short one is there for the address a human
            // types.
            let mut addresses = Vec::with_capacity(2);
            if let Some(address) = zone.address(&env.app_id, &env.environment) {
                addresses.push(address);
            }
            if env.environment == DEFAULT_ENVIRONMENT {
                let long = format!("{}{PREVIEW_SEPARATOR}{}", env.environment, env.app_id);
                if is_dns_label(&long) {
                    addresses.push(format!("{long}.{}", zone.as_str()));
                }
            }
            if addresses.is_empty() {
                warn!(
                    app_id = %env.app_id,
                    environment = %env.environment,
                    zone = %zone.as_str(),
                    "no automatic address for this environment: its name cannot be a DNS \
                     label, which takes letters, digits and '-' only. It is reachable \
                     through whatever route was created for it, and nowhere else"
                );
                continue;
            }
            for address in addresses {
                if let Some(previous) = table.preview.insert(address.clone(), target.clone())
                    && previous != target
                {
                    warn!(
                        address = %address,
                        kept = %format!("{}/{}", target.app_id, target.environment),
                        dropped = %format!("{}/{}", previous.app_id, previous.environment),
                        "two environments want the same automatic address; the later one \
                         wins and the other has none. Rename one of them, or give it an \
                         explicit route"
                    );
                }
            }
        }

        table
    }

    /// The version this table was built from. Empty means no control plane
    /// answer has ever been applied.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Unix seconds the control plane built the document behind this table.
    pub fn generated_at(&self) -> i64 {
        self.generated_at
    }

    /// Whether this table can answer anything at all.
    pub fn is_empty(&self) -> bool {
        self.by_host.is_empty()
            && self.wildcards.is_empty()
            && self.any_host.is_empty()
            && self.preview.is_empty()
    }

    pub fn route_count(&self) -> usize {
        self.route_count
    }

    pub fn environment_count(&self) -> usize {
        self.environment_count
    }

    pub fn preview_count(&self) -> usize {
        self.preview.len()
    }

    /// Which application and environment serves `host` + `path`.
    pub fn match_request(&self, host: &str, path: &str) -> Option<Match> {
        let host = normalize_host(host);

        if let Some(candidates) = self.by_host.get(&host)
            && let Some(route) = candidates.iter().find(|route| route.covers(path))
        {
            return Some(Match {
                target: route.target.clone(),
                source: MatchSource::Route,
            });
        }

        for (suffix, route) in &self.wildcards {
            if host.len() > suffix.len() && host.ends_with(suffix.as_str()) && route.covers(path) {
                return Some(Match {
                    target: route.target.clone(),
                    source: MatchSource::Route,
                });
            }
        }

        if let Some(target) = self.preview.get(&host) {
            return Some(Match {
                target: target.clone(),
                source: MatchSource::Preview,
            });
        }

        self.any_host
            .iter()
            .find(|route| route.covers(path))
            .map(|route| Match {
                target: route.target.clone(),
                source: MatchSource::Route,
            })
    }

    /// The default environment of an application the catalog knows, for a
    /// request that named the application by header instead of by host.
    ///
    /// The header names an application, not an environment, so there is only
    /// one honest answer: the default one. An application whose catalog has
    /// no default environment is not resolvable this way at all.
    pub fn target_by_app_id(&self, app_id: &str) -> Option<Target> {
        self.apps_with_default_env.contains(app_id).then(|| Target {
            app_id: app_id.to_string(),
            environment: DEFAULT_ENVIRONMENT.to_string(),
        })
    }

    /// Every rule in this table, one canonical line each.
    ///
    /// This is what the change log is diffed on: an operator asking "why did
    /// that request go there" needs the lines that appeared and disappeared,
    /// not a pair of counts.
    pub fn describe(&self) -> BTreeSet<String> {
        let mut lines = BTreeSet::new();
        for (host, candidates) in &self.by_host {
            for route in candidates {
                lines.insert(describe_route(host, route));
            }
        }
        for (suffix, route) in &self.wildcards {
            lines.insert(describe_route(&format!("*{suffix}"), route));
        }
        for route in &self.any_host {
            lines.insert(describe_route("*", route));
        }
        for (host, target) in &self.preview {
            lines.insert(format!(
                "{host}/ -> {}/{} (automatic)",
                target.app_id, target.environment
            ));
        }
        lines
    }
}

fn describe_route(host: &str, route: &CompiledRoute) -> String {
    format!(
        "{host}{} -> {}/{} (priority {})",
        route.path_prefix, route.target.app_id, route.target.environment, route.priority
    )
}

fn sort_candidates(candidates: &mut [CompiledRoute]) {
    candidates.sort_by(candidate_order);
}

/// Highest priority first, then longest prefix, then a stable tie-break so
/// two tables with the same content always match the same way.
fn candidate_order(a: &CompiledRoute, b: &CompiledRoute) -> std::cmp::Ordering {
    b.priority
        .cmp(&a.priority)
        .then_with(|| b.path_prefix.len().cmp(&a.path_prefix.len()))
        .then_with(|| a.path_prefix.cmp(&b.path_prefix))
        .then_with(|| a.target.app_id.cmp(&b.target.app_id))
        .then_with(|| a.target.environment.cmp(&b.target.environment))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(json: &str) -> RoutingTableDocument {
        let doc: RoutingTableDocument = serde_json::from_str(json).unwrap();
        doc.validate().unwrap();
        doc
    }

    fn table(json: &str, zone: Option<&str>) -> CatalogRoutes {
        let zone = zone.and_then(PreviewZone::new);
        CatalogRoutes::build(&document(json), zone.as_ref())
    }

    const SAMPLE: &str = r#"{
        "version": "v1",
        "generated_at": 10,
        "routes": [
            {"app_id":"api","environment":"production","host":"example.com","path_prefix":"/api","priority":0},
            {"app_id":"site","environment":"production","host":"example.com","path_prefix":"/","priority":0},
            {"app_id":"deep","environment":"production","host":"example.com","path_prefix":"/api/v2","priority":0},
            {"app_id":"any","environment":"production","host":"*.example.com","path_prefix":"/","priority":0},
            {"app_id":"eu","environment":"production","host":"*.eu.example.com","path_prefix":"/","priority":0},
            {"app_id":"fallback","environment":"production","path_prefix":"/","priority":0}
        ],
        "environments": [
            {"app_id":"api","environment":"production"},
            {"app_id":"api","environment":"pr-42"}
        ]
    }"#;

    fn hit(table: &CatalogRoutes, host: &str, path: &str) -> String {
        let found = table
            .match_request(host, path)
            .unwrap_or_else(|| panic!("no match for {host}{path}"));
        format!("{}/{}", found.target.app_id, found.target.environment)
    }

    #[test]
    fn a_path_prefix_route_actually_routes() {
        let table = table(SAMPLE, None);
        assert_eq!(hit(&table, "example.com", "/api/users"), "api/production");
        assert_eq!(hit(&table, "example.com", "/api"), "api/production");
        assert_eq!(hit(&table, "example.com", "/other"), "site/production");
        // A longer prefix wins at equal priority.
        assert_eq!(hit(&table, "example.com", "/api/v2/x"), "deep/production");
        // And a prefix only matches on a segment boundary.
        assert_eq!(hit(&table, "example.com", "/apiary"), "site/production");
    }

    #[test]
    fn priority_beats_prefix_length() {
        let table = table(
            r#"{"version":"v","routes":[
                {"app_id":"long","environment":"production","host":"h.test","path_prefix":"/a/b","priority":0},
                {"app_id":"short","environment":"production","host":"h.test","path_prefix":"/","priority":5}
            ],"environments":[]}"#,
            None,
        );
        assert_eq!(hit(&table, "h.test", "/a/b/c"), "short/production");
    }

    #[test]
    fn host_specificity_runs_exact_then_wildcard_then_any() {
        let table = table(SAMPLE, None);
        assert_eq!(hit(&table, "example.com:8080", "/"), "site/production");
        assert_eq!(hit(&table, "shop.example.com", "/"), "any/production");
        assert_eq!(hit(&table, "a.eu.example.com", "/"), "eu/production");
        assert_eq!(hit(&table, "somewhere.else", "/"), "fallback/production");
    }

    #[test]
    fn a_preview_address_is_synthesised_per_environment() {
        let table = table(SAMPLE, Some("preview.test"));
        assert_eq!(hit(&table, "pr-42--api.preview.test", "/"), "api/pr-42");
        // The default environment answers on both forms.
        assert_eq!(hit(&table, "api.preview.test", "/"), "api/production");
        assert_eq!(
            hit(&table, "production--api.preview.test", "/"),
            "api/production"
        );
        assert_eq!(
            table
                .match_request("pr-42--api.preview.test", "/")
                .unwrap()
                .source,
            MatchSource::Preview
        );
    }

    #[test]
    fn turning_the_zone_off_removes_every_automatic_address() {
        let table = table(SAMPLE, None);
        assert_eq!(table.preview_count(), 0);
        // Only the "any domain" catch-all is left to answer it.
        assert_eq!(
            hit(&table, "pr-42--api.preview.test", "/"),
            "fallback/production"
        );
    }

    #[test]
    fn an_explicit_route_beats_the_automatic_address() {
        let table = table(
            r#"{"version":"v","routes":[
                {"app_id":"claimed","environment":"production","host":"pr-42--api.preview.test","path_prefix":"/","priority":0}
            ],"environments":[{"app_id":"api","environment":"pr-42"}]}"#,
            Some("preview.test"),
        );
        assert_eq!(
            hit(&table, "pr-42--api.preview.test", "/"),
            "claimed/production"
        );
    }

    #[test]
    fn a_name_that_cannot_be_a_hostname_gets_no_automatic_address() {
        // '_' is legal in an app id (it is half of a storage tenant id) and
        // illegal in a hostname.
        let table = table(
            r#"{"version":"v","routes":[],"environments":[{"app_id":"my_app","environment":"production"}]}"#,
            Some("preview.test"),
        );
        assert_eq!(table.preview_count(), 0);
        assert!(table.match_request("my_app.preview.test", "/").is_none());
    }

    #[test]
    fn the_header_path_resolves_to_the_default_environment_only() {
        let table = table(SAMPLE, None);
        assert_eq!(
            table.target_by_app_id("api"),
            Some(Target {
                app_id: "api".to_string(),
                environment: "production".to_string()
            })
        );
        assert_eq!(table.target_by_app_id("nothing-like-it"), None);
    }

    #[test]
    fn describe_is_stable_and_diffable() {
        let before = table(SAMPLE, Some("preview.test"));
        let after = table(
            &SAMPLE.replace(
                r#""path_prefix":"/api","priority":0"#,
                r#""path_prefix":"/api","priority":9"#,
            ),
            Some("preview.test"),
        );
        let added: Vec<_> = after
            .describe()
            .difference(&before.describe())
            .cloned()
            .collect();
        assert_eq!(
            added,
            vec!["example.com/api -> api/production (priority 9)".to_string()]
        );
    }

    #[test]
    fn an_empty_table_is_empty() {
        let table = table(r#"{"version":"v","routes":[],"environments":[]}"#, None);
        assert!(table.is_empty());
        assert_eq!(table.version(), "v");
        assert!(CatalogRoutes::default().version().is_empty());
    }

    #[test]
    fn a_zone_is_normalised_or_refused() {
        assert_eq!(
            PreviewZone::new(" .Preview.Test. ").unwrap().as_str(),
            "preview.test"
        );
        assert!(PreviewZone::new("   ").is_none());
        assert!(PreviewZone::new("...").is_none());
    }
}
