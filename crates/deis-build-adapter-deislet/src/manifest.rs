//! Building `manifest.json`.
//!
//! The manifest is the only thing the control plane reads out of a bundle and
//! the only thing a node has before it loads one, so it is built from
//! [`deis_bundle::Manifest`] rather than from a local struct or a hand-written
//! `json!`. A second definition of this shape would drift from the one the
//! control plane parses, and the symptom - a bundle that uploads and then does
//! not run - would show up a long way from the cause.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use deis_build_core::ProjectConfig;
use deis_bundle::{
    Asset, DeclaredCompatibility, DeclaredLimits, Manifest, QueueConsumer, Route, SUPPORTED_SCHEMA,
};

use crate::assets::StagedAsset;
use crate::routing::RouteEntry;
use crate::site::{build_site_config, is_site_file};

/// Where the bundled entry module sits inside the archive.
///
/// Re-exported from [`deis_bundle`], which is where the archive's layout is
/// defined: one definition, shared with the control plane that verifies a
/// bundle and the node that loads one.
pub use deis_bundle::WORKER_ENTRY;

/// The execution profile a node must provide to run this bundle.
///
/// Not the project's `runtime` field: that one says what the source is written
/// against ("typescript", "node"), while this says which set of globals and
/// bindings deis-runtime has to hand the isolate. There is one profile today.
pub const RUNTIME_PROFILE: &str = "standard-v1";

/// The four services a bundle can declare. Named exactly as they appear on
/// `context.services` in a handler, so a developer reading the manifest sees
/// the names they wrote.
const SERVICE_NAMES: [&str; 4] = ["kv", "database", "blob", "queue"];

/// Assemble the manifest for one build.
pub fn build_manifest(
    config: &ProjectConfig,
    routes: &[RouteEntry],
    assets: &[StagedAsset],
) -> Result<Manifest> {
    let app = config.name.trim();
    if app.is_empty() {
        bail!("the project's \"name\" is the application id the bundle is filed under; it cannot be empty");
    }

    // Read and checked before anything else uses `assets`, because the two
    // files it reads are then dropped from the list: they describe the site,
    // they are not part of it, and a bundle that served `/_redirects` would
    // publish an author's routing table.
    let site = build_site_config(config, assets)?;

    let manifest = Manifest {
        schema: SUPPORTED_SCHEMA,
        app: app.to_string(),
        entry: WORKER_ENTRY.to_string(),
        runtime: Some(RUNTIME_PROFILE.to_string()),
        services: services(config),
        env: environment(config),
        secrets: secret_names(config),
        routes: routes
            .iter()
            .map(|route| Route {
                pattern: route.pattern.clone(),
                // The file tree says which path a handler answers, not which
                // methods: an edge-canon handler takes every method and
                // decides for itself. An empty list is the manifest's way of
                // saying "any method", which is exactly right here.
                methods: Vec::new(),
            })
            .collect(),
        cron: config.cron.clone(),
        queues: config
            .queues
            .iter()
            .map(|queue| QueueConsumer {
                name: queue.name.clone(),
                batch_size: queue.batch_size,
            })
            .collect(),
        assets: assets
            .iter()
            .filter(|asset| !is_site_file(&asset.path))
            .map(|asset| Asset {
                path: asset.path.clone(),
                sha256: Some(asset.sha256.clone()),
                content_type: Some(asset.content_type.to_string()),
                size: Some(asset.size()),
            })
            .collect(),
        site,
        limits: limits(config)?,
        compatibility: compatibility(config)?,
    };

    // Fail here rather than at upload: the checks are the control plane's, and
    // finding out about a bad asset path when the build runs is far cheaper
    // than finding out when a deploy is rejected.
    manifest.validate()?;

    Ok(manifest)
}

/// The ceilings the project asks to be held to, translated into the names the
/// manifest, the delivery message and the node all use.
///
/// Checked here, with the project file still on the author's screen, rather
/// than left to the upload: `deis-control` refuses a block no node could act
/// on, and finding that out from a rejected deploy - after a build, an upload
/// and a wait - is a worse way to learn that a timeout of zero is not a
/// timeout.
///
/// A project that declares nothing, or a `"limits": {}` that says nothing,
/// leaves the manifest without the key at all: there is no ceiling to record,
/// and writing an empty object would put a "declares limits" line in the log
/// of every delivery of it.
fn limits(config: &ProjectConfig) -> Result<Option<DeclaredLimits>> {
    let Some(declared) = config.limits.as_ref() else {
        return Ok(None);
    };
    let limits = DeclaredLimits {
        memory_mb: declared.memory_mb,
        timeout_ms: declared.timeout_ms,
        rate_limit_ops_per_sec: declared.rate_limit_ops_per_sec,
        cpu_time_limit_ms: declared.cpu_time_limit_ms,
        resource_table_max: declared.resource_table_max,
    };
    if limits.is_empty() {
        return Ok(None);
    }
    limits
        .validate()
        .map_err(|e| anyhow::anyhow!("the project's \"limits\" block is not usable: {e}"))?;
    Ok(Some(limits))
}

/// The day this project says it was written against, and the behaviours it
/// takes early or holds back by name.
///
/// Checked here, with the project file still on the author's screen: a date
/// this build of the platform does not know, or a flag name nothing knows,
/// fails the build rather than producing a bundle every node would refuse. The
/// check itself is `deis_bundle`'s, so the build, the control plane and the
/// node all refuse exactly the same declarations.
///
/// A project that names neither leaves the key out of the manifest entirely,
/// which is what pins it to the oldest behaviour the platform has. Nothing is
/// filled in on the author's behalf: a date this build chose would move with
/// the calendar, and the same unchanged source would then compile to two
/// bundles that behave differently. `deis deploy` is where an author is told
/// their application has no date, once per deploy, with the date to write.
fn compatibility(config: &ProjectConfig) -> Result<Option<DeclaredCompatibility>> {
    let date = match config.compatibility_date.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(text) => Some(text.parse().map_err(|e| {
            anyhow::anyhow!("the project's \"compatibilityDate\" is not usable: {e}")
        })?),
    };
    let flags: Vec<String> = config
        .compatibility_flags
        .iter()
        .map(|flag| flag.trim().to_string())
        .filter(|flag| !flag.is_empty())
        .collect();

    let declared = DeclaredCompatibility { date, flags };
    if declared.is_empty() {
        return Ok(None);
    }
    declared.validate().map_err(|e| {
        anyhow::anyhow!("the project's compatibility declaration is not usable: {e}")
    })?;
    Ok(Some(declared))
}

/// Which platform services the app declares it uses.
///
/// All four are written out, including the false ones, so the manifest reads
/// as a complete answer rather than leaving a reader to wonder whether a
/// missing key means "no" or "the builder forgot".
fn services(config: &ProjectConfig) -> BTreeMap<String, bool> {
    let declared = config.services.as_ref();
    let enabled = |service: Option<&deis_build_core::ServiceConfig>| {
        service.map(|s| s.enabled).unwrap_or(false)
    };

    SERVICE_NAMES
        .iter()
        .map(|name| {
            let on = match *name {
                "kv" => enabled(declared.and_then(|s| s.kv.as_ref())),
                "database" => enabled(declared.and_then(|s| s.database.as_ref())),
                "blob" => enabled(declared.and_then(|s| s.blob.as_ref())),
                "queue" => enabled(declared.and_then(|s| s.queue.as_ref())),
                _ => false,
            };
            (name.to_string(), on)
        })
        .collect()
}

fn environment(config: &ProjectConfig) -> BTreeMap<String, String> {
    config
        .environment
        .as_ref()
        .and_then(|env| env.variables.as_ref())
        .map(|vars| {
            vars.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Secret *names*, sorted and deduplicated. Values never enter a bundle: the
/// control plane holds them and hands them to a node at deploy time.
fn secret_names(config: &ProjectConfig) -> Vec<String> {
    let mut names: Vec<String> = config
        .environment
        .as_ref()
        .and_then(|env| env.secrets.as_ref())
        .map(|secrets| {
            secrets
                .iter()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: serde_json::Value) -> ProjectConfig {
        let mut json = serde_json::json!({
            "version": "1.0",
            "name": "demo",
            "runtime": "standard-v1",
            "language": "typescript"
        });
        for (key, value) in extra.as_object().expect("an object").iter() {
            json[key] = value.clone();
        }
        serde_json::from_value(json).expect("config parses")
    }

    fn route(pattern: &str) -> RouteEntry {
        RouteEntry {
            pattern: pattern.to_string(),
            import_path: "../src/x.ts".to_string(),
            wildcard: None,
        }
    }

    #[test]
    fn the_entry_points_at_the_bundled_worker() {
        let manifest = build_manifest(&config(serde_json::json!({})), &[], &[]).unwrap();
        assert_eq!(manifest.entry, WORKER_ENTRY);
        assert_eq!(manifest.schema, SUPPORTED_SCHEMA);
        assert_eq!(manifest.app, "demo");
        assert_eq!(manifest.runtime.as_deref(), Some(RUNTIME_PROFILE));
    }

    #[test]
    fn every_service_is_answered_yes_or_no() {
        let manifest = build_manifest(
            &config(serde_json::json!({
                "services": {
                    "kv": { "enabled": true },
                    "database": { "enabled": true },
                    "blob": { "enabled": false }
                }
            })),
            &[],
            &[],
        )
        .unwrap();

        assert!(manifest.uses_service("kv"));
        assert!(manifest.uses_service("database"));
        assert!(!manifest.uses_service("blob"));
        // Undeclared, but still written down as a no.
        assert_eq!(manifest.services.get("queue"), Some(&false));
    }

    #[test]
    fn secret_values_never_reach_the_manifest() {
        let manifest = build_manifest(
            &config(serde_json::json!({
                "environment": {
                    "variables": { "LOG_LEVEL": "info" },
                    "secrets": ["API_KEY", "API_KEY", " DB_PASSWORD "]
                }
            })),
            &[],
            &[],
        )
        .unwrap();

        assert_eq!(manifest.secrets, vec!["API_KEY", "DB_PASSWORD"]);
        assert_eq!(
            manifest.env.get("LOG_LEVEL").map(String::as_str),
            Some("info")
        );
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(
            !json.contains("password"),
            "no value ever accompanies a name"
        );
    }

    #[test]
    fn cron_and_queues_are_carried_through_verbatim() {
        let manifest = build_manifest(
            &config(serde_json::json!({
                "cron": ["0 */6 * * *"],
                "queues": [{ "name": "jobs", "batchSize": 10 }]
            })),
            &[],
            &[],
        )
        .unwrap();

        assert_eq!(manifest.cron, vec!["0 */6 * * *"]);
        assert_eq!(manifest.queues[0].name, "jobs");
        assert_eq!(manifest.queues[0].batch_size, Some(10));
    }

    #[test]
    fn a_compatibility_date_and_its_flags_reach_the_manifest() {
        let manifest = build_manifest(
            &config(serde_json::json!({
                "compatibilityDate": "2026-09-01",
                "compatibilityFlags": ["single_set_cookie_header"]
            })),
            &[],
            &[],
        )
        .unwrap();

        let declared = manifest.compatibility.as_ref().expect("declared");
        assert_eq!(
            declared.date.map(|d| d.to_string()).as_deref(),
            Some("2026-09-01")
        );
        assert_eq!(declared.flags, vec!["single_set_cookie_header"]);
        // The flag holds the behaviour back past the date that would have
        // turned it on, which is the whole point of naming one.
        let profile = manifest.compatibility_profile().unwrap();
        assert!(!profile.is_on(deis_bundle::Flag::MultipleSetCookieHeaders));
    }

    #[test]
    fn a_project_that_says_nothing_leaves_the_key_out() {
        let manifest = build_manifest(&config(serde_json::json!({})), &[], &[]).unwrap();
        assert!(
            manifest.compatibility.is_none(),
            "an empty block would put a compatibility line in every log of every delivery"
        );
        assert_eq!(manifest.compatibility_profile().unwrap().date(), None);
    }

    /// The failure this check exists to prevent: a build that produced a
    /// bundle no node could load, discovered after an upload and a wait.
    #[test]
    fn a_date_this_build_cannot_honour_fails_the_build() {
        let err = build_manifest(
            &config(serde_json::json!({ "compatibilityDate": "2099-01-01" })),
            &[],
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("2099-01-01"), "{err}");
        assert!(
            err.contains(&deis_bundle::PLATFORM_KNOWN_THROUGH.to_string()),
            "{err}"
        );
    }

    #[test]
    fn a_misspelled_date_fails_the_build_rather_than_being_dropped() {
        let err = build_manifest(
            &config(serde_json::json!({ "compatibilityDate": "2026-9-1" })),
            &[],
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("YYYY-MM-DD"), "{err}");
    }

    #[test]
    fn a_flag_nobody_knows_fails_the_build() {
        let err = build_manifest(
            &config(serde_json::json!({ "compatibilityFlags": ["all_the_cookies"] })),
            &[],
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("all_the_cookies"), "{err}");
        assert!(err.contains("multiple_set_cookie_headers"), "{err}");
    }

    #[test]
    fn a_route_matches_any_method() {
        let manifest =
            build_manifest(&config(serde_json::json!({})), &[route("/users/:id")], &[]).unwrap();
        assert_eq!(manifest.route_patterns(), vec!["/users/:id"]);
        assert!(
            manifest.routes[0].methods.is_empty(),
            "an empty method list is the manifest's \"any method\""
        );
    }

    #[test]
    fn an_asset_carries_what_it_takes_to_serve_it_unopened() {
        let asset = StagedAsset {
            path: "css/app.css".to_string(),
            bytes: b"body{}".to_vec(),
            sha256: "abc".to_string(),
            content_type: "text/css; charset=utf-8",
        };
        let manifest = build_manifest(&config(serde_json::json!({})), &[], &[asset]).unwrap();

        let entry = &manifest.assets[0];
        assert_eq!(entry.path, "css/app.css");
        assert_eq!(entry.sha256.as_deref(), Some("abc"));
        assert_eq!(
            entry.content_type.as_deref(),
            Some("text/css; charset=utf-8")
        );
        assert_eq!(entry.size, Some(6));
    }

    fn staged(path: &str, body: &str) -> StagedAsset {
        StagedAsset {
            path: path.to_string(),
            bytes: body.as_bytes().to_vec(),
            sha256: "0".repeat(64),
            content_type: "text/plain; charset=utf-8",
        }
    }

    #[test]
    fn the_two_files_a_site_configures_itself_with_are_read_not_served() {
        let manifest = build_manifest(
            &config(serde_json::json!({})),
            &[],
            &[
                staged("_redirects", "/old  /new  301\n"),
                staged("_headers", "/*\n  X-Frame-Options: DENY\n"),
                staged("index.html", "<h1>hi</h1>"),
            ],
        )
        .unwrap();

        let paths: Vec<&str> = manifest.assets.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["index.html"],
            "a visitor asking for /_redirects must not get the routing table"
        );
        assert_eq!(manifest.site.redirects.len(), 1);
        assert_eq!(manifest.site.headers.len(), 1);
    }

    #[test]
    fn a_bad_redirect_line_fails_the_build_with_its_line_number() {
        let err = build_manifest(
            &config(serde_json::json!({})),
            &[],
            &[staged("_redirects", "/a  /b  301\n/c  /d  999\n")],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("999"), "{err}");
    }

    #[test]
    fn a_rewrite_target_that_is_not_in_the_bundle_fails_the_build() {
        // The rule would match every request and then serve nothing, which is
        // a site that 404s everywhere - discovered by a visitor, not the
        // author, unless the build says so.
        let err = build_manifest(
            &config(serde_json::json!({})),
            &[],
            &[
                staged("_redirects", "/app/*  /app/index.html  200\n"),
                staged("index.html", "<h1>hi</h1>"),
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("/app/index.html"), "{err}");
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn shipping_a_404_page_is_enough_to_get_it_served() {
        let manifest = build_manifest(
            &config(serde_json::json!({})),
            &[],
            &[staged("404.html", "gone"), staged("index.html", "hi")],
        )
        .unwrap();
        assert_eq!(manifest.site.not_found, deis_bundle::NotFound::Page);
    }

    #[test]
    fn a_site_with_no_404_page_leaves_the_answer_to_the_app() {
        let manifest = build_manifest(
            &config(serde_json::json!({})),
            &[],
            &[staged("index.html", "hi")],
        )
        .unwrap();
        assert_eq!(manifest.site.not_found, deis_bundle::NotFound::None);
    }

    #[test]
    fn an_author_can_ask_for_the_spa_fallback_instead() {
        let manifest = build_manifest(
            &config(serde_json::json!({
                "assets": { "dir": "./public", "notFound": "single-page-app" }
            })),
            &[],
            &[staged("404.html", "gone"), staged("index.html", "hi")],
        )
        .unwrap();
        assert_eq!(
            manifest.site.not_found,
            deis_bundle::NotFound::SinglePageApp,
            "a declared choice wins over the one inferred from the files"
        );
    }

    #[test]
    fn an_author_can_turn_the_inferred_404_page_off() {
        let manifest = build_manifest(
            &config(serde_json::json!({
                "assets": { "dir": "./public", "notFound": "none" }
            })),
            &[],
            &[staged("404.html", "gone")],
        )
        .unwrap();
        assert_eq!(manifest.site.not_found, deis_bundle::NotFound::None);
    }

    #[test]
    fn asking_for_a_fallback_whose_page_is_missing_fails_the_build() {
        let err = build_manifest(
            &config(serde_json::json!({
                "assets": { "dir": "./public", "notFound": "single-page-app" }
            })),
            &[],
            &[staged("about.html", "hi")],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("index.html"), "{err}");
    }

    #[test]
    fn a_header_the_platform_owns_fails_the_build() {
        let err = build_manifest(
            &config(serde_json::json!({})),
            &[],
            &[staged("_headers", "/*\n  Content-Length: 0\n")],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("may not set"), "{err}");
    }

    #[test]
    fn an_app_without_a_name_is_refused() {
        // The name is the app id the version is filed under; an empty one
        // would upload into nothing.
        let mut config = config(serde_json::json!({}));
        config.name = "   ".to_string();
        let err = build_manifest(&config, &[], &[]).unwrap_err();
        assert!(err.to_string().contains("cannot be empty"), "{err}");
    }
}
