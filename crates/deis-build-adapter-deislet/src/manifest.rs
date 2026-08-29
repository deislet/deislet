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
use deis_bundle::{Asset, Manifest, QueueConsumer, Route, SUPPORTED_SCHEMA};

use crate::assets::StagedAsset;
use crate::routing::RouteEntry;

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
            .map(|asset| Asset {
                path: asset.path.clone(),
                sha256: Some(asset.sha256.clone()),
                content_type: Some(asset.content_type.to_string()),
                size: Some(asset.size()),
            })
            .collect(),
    };

    // Fail here rather than at upload: the checks are the control plane's, and
    // finding out about a bad asset path when the build runs is far cheaper
    // than finding out when a deploy is rejected.
    manifest.validate()?;

    Ok(manifest)
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
