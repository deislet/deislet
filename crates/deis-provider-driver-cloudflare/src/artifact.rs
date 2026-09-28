//! The Cloudflare build artifact: reading its binding plan, and turning its
//! `wrangler.toml` into one wrangler can deploy.
//!
//! The build adapter (`deis-build-adapter-cloudflare`) writes two files the
//! driver consumes: `binding-plan.json`, the machine-readable list of logical
//! resources, and `wrangler.toml`, which carries `deis:unresolved/<kind>/<name>`
//! placeholders wherever wrangler wants a resource identifier and
//! `deis:unresolved/config/<NAME>` for deployment-time `[vars]`.
//!
//! [`render_wrangler_toml`] rewrites a copy of `wrangler.toml` structurally:
//! the Worker `name` becomes the environment's scoped Worker name, every
//! resource section gets the resolved Cloudflare identifier or scoped name,
//! queue consumers point at the scoped queue, and `[vars]` get their values.
//! Any section that does not match the plan, and any placeholder left over,
//! refuses the artifact - the driver never deploys a placeholder. It also
//! sets `preview_urls = false`: a version that is uploaded but not yet
//! activated must not be reachable on a workers.dev preview URL.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use deis_build_core::{
    BINDING_PLAN_FORMAT, BINDING_PLAN_TARGET_CLOUDFLARE, BindingKind, PlannedBinding,
    PlannedQueueConsumer, UNRESOLVED_RESOURCE_REF_PREFIX, unresolved_resource_ref,
};
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, value};

use crate::naming::DeploymentScope;

pub const BINDING_PLAN_FILE: &str = "binding-plan.json";
pub const WRANGLER_TOML_FILE: &str = "wrangler.toml";
/// The placeholder prefix the adapter uses for deployment-time config values.
pub const UNRESOLVED_CONFIG_PREFIX: &str = "deis:unresolved/config/";

/// `binding-plan.json`, as the adapter writes it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingPlanDocument {
    pub format: String,
    pub target: String,
    pub bindings: Vec<PlannedBinding>,
    pub queue_consumers: Vec<PlannedQueueConsumer>,
    pub crons: Vec<String>,
}

/// One Cloudflare resource the deployment needs.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RequiredResource {
    pub kind: BindingKind,
    /// The logical name: a binding name, or a consumed queue's name.
    pub logical: String,
    /// The scoped Cloudflare name.
    pub name: String,
}

impl RequiredResource {
    /// The key receipts and the journal use: `kv:HOT`, `d1:DB`, `r2:FILES`,
    /// `q:ORDERS`.
    pub fn key(&self) -> String {
        format!("{}:{}", crate::naming::kind_token(self.kind), self.logical)
    }
}

impl BindingPlanDocument {
    pub fn load(artifact_dir: &Path) -> Result<Self, ArtifactError> {
        let path = artifact_dir.join(BINDING_PLAN_FILE);
        let contents = fs::read_to_string(&path).map_err(|error| ArtifactError::Missing {
            file: path.display().to_string(),
            message: error.to_string(),
        })?;
        let plan: Self =
            serde_json::from_str(&contents).map_err(|error| ArtifactError::Invalid {
                message: format!("{BINDING_PLAN_FILE} is not a binding plan: {error}"),
            })?;
        if plan.format != BINDING_PLAN_FORMAT {
            return Err(ArtifactError::Invalid {
                message: format!(
                    "{BINDING_PLAN_FILE} has format {:?}; this driver reads {BINDING_PLAN_FORMAT:?}",
                    plan.format
                ),
            });
        }
        if plan.target != BINDING_PLAN_TARGET_CLOUDFLARE {
            return Err(ArtifactError::Invalid {
                message: format!(
                    "{BINDING_PLAN_FILE} was built for target {:?}, not {BINDING_PLAN_TARGET_CLOUDFLARE:?}",
                    plan.target
                ),
            });
        }
        for binding in &plan.bindings {
            let expected = unresolved_resource_ref(binding.kind, &binding.name);
            if binding.provider_resource_ref != expected {
                return Err(ArtifactError::Invalid {
                    message: format!(
                        "binding {:?} carries the reference {:?}; a build emits {expected:?}",
                        binding.name, binding.provider_resource_ref
                    ),
                });
            }
        }
        Ok(plan)
    }

    /// Every resource to provision: one per binding, plus one queue per
    /// consumed queue that no producer binding already names. Refuses two
    /// logical names that would share a Cloudflare name.
    pub fn required_resources(
        &self,
        scope: &DeploymentScope,
    ) -> Result<Vec<RequiredResource>, ArtifactError> {
        let mut resources = BTreeMap::new();
        let mut add = |kind: BindingKind, logical: &str| {
            resources
                .entry((kind, logical.to_owned()))
                .or_insert_with(|| RequiredResource {
                    kind,
                    logical: logical.to_owned(),
                    name: scope.resource_name(kind, logical),
                });
        };
        for binding in &self.bindings {
            add(binding.kind, &binding.name);
        }
        for consumer in &self.queue_consumers {
            add(BindingKind::Queue, &consumer.queue);
        }
        let resources = resources.into_values().collect::<Vec<_>>();
        let mut seen = BTreeMap::new();
        for resource in &resources {
            if let Some(other) =
                seen.insert((resource.kind, resource.name.clone()), &resource.logical)
            {
                return Err(ArtifactError::Invalid {
                    message: format!(
                        "logical names {other:?} and {:?} both map to the Cloudflare name {:?}; \
                         rename one of them",
                        resource.logical, resource.name
                    ),
                });
            }
        }
        Ok(resources)
    }
}

/// The values a rendered `wrangler.toml` is filled with.
pub struct RenderInputs<'a> {
    pub worker_name: &'a str,
    /// Resolved identifiers by [`RequiredResource::key`]: the namespace id
    /// for KV, the database uuid for D1, the bucket name for R2, the queue
    /// name for queues.
    pub resolved: &'a BTreeMap<String, String>,
    /// Scoped names by [`RequiredResource::key`].
    pub names: &'a BTreeMap<String, String>,
    /// Deployment config values for `[vars]`.
    pub config: &'a BTreeMap<String, String>,
}

/// Rewrite the adapter's `wrangler.toml` for deployment.
pub fn render_wrangler_toml(
    source: &str,
    plan: &BindingPlanDocument,
    inputs: &RenderInputs<'_>,
) -> Result<String, ArtifactError> {
    let mut doc: DocumentMut = source.parse().map_err(|error| ArtifactError::Invalid {
        message: format!("{WRANGLER_TOML_FILE} is not valid TOML: {error}"),
    })?;
    doc["name"] = value(inputs.worker_name);

    let lookup = |map: &BTreeMap<String, String>, kind: BindingKind, logical: &str| {
        let key = format!("{}:{logical}", crate::naming::kind_token(kind));
        map.get(&key)
            .cloned()
            .ok_or_else(|| ArtifactError::Unresolved {
                placeholder: unresolved_resource_ref(kind, logical),
            })
    };

    let planned = plan
        .bindings
        .iter()
        .map(|binding| (binding.name.as_str(), binding.kind))
        .collect::<BTreeMap<_, _>>();
    let mut rewritten = BTreeSet::new();

    let sections: [Section; 4] = [
        (
            &["kv_namespaces"],
            BindingKind::Kv,
            "binding",
            &[("id", Field::Resolved)],
        ),
        (
            &["d1_databases"],
            BindingKind::Database,
            "binding",
            &[
                ("database_id", Field::Resolved),
                ("database_name", Field::Name),
            ],
        ),
        (
            &["r2_buckets"],
            BindingKind::Blob,
            "binding",
            &[("bucket_name", Field::Resolved)],
        ),
        (
            &["queues", "producers"],
            BindingKind::Queue,
            "binding",
            &[("queue", Field::Resolved)],
        ),
    ];

    for (path, kind, binding_key, fields) in sections {
        let Some(tables) = array_of_tables(&mut doc, path) else {
            continue;
        };
        for table in tables.iter_mut() {
            let binding = table
                .get(binding_key)
                .and_then(Item::as_str)
                .ok_or_else(|| ArtifactError::Invalid {
                    message: format!("a [[{}]] entry has no {binding_key}", path.join(".")),
                })?
                .to_owned();
            if planned.get(binding.as_str()) != Some(&kind) {
                return Err(ArtifactError::Invalid {
                    message: format!(
                        "[[{}]] binds {binding:?}, which binding-plan.json does not plan as {}",
                        path.join("."),
                        kind.as_str()
                    ),
                });
            }
            for (field, source) in fields {
                let replacement = match source {
                    Field::Resolved => lookup(inputs.resolved, kind, &binding)?,
                    Field::Name => lookup(inputs.names, kind, &binding)?,
                };
                table[*field] = value(replacement);
            }
            rewritten.insert((kind, binding));
        }
    }

    for binding in &plan.bindings {
        if !rewritten.contains(&(binding.kind, binding.name.clone())) {
            return Err(ArtifactError::Invalid {
                message: format!(
                    "binding-plan.json plans {} {:?}, but {WRANGLER_TOML_FILE} has no section for it",
                    binding.kind.as_str(),
                    binding.name
                ),
            });
        }
    }

    if let Some(consumers) = array_of_tables(&mut doc, &["queues", "consumers"]) {
        for table in consumers.iter_mut() {
            let queue = table
                .get("queue")
                .and_then(Item::as_str)
                .ok_or_else(|| ArtifactError::Invalid {
                    message: "a [[queues.consumers]] entry has no queue".to_owned(),
                })?
                .to_owned();
            if !plan
                .queue_consumers
                .iter()
                .any(|consumer| consumer.queue == queue)
            {
                return Err(ArtifactError::Invalid {
                    message: format!(
                        "[[queues.consumers]] consumes {queue:?}, which binding-plan.json does \
                         not plan"
                    ),
                });
            }
            table["queue"] = value(lookup(inputs.names, BindingKind::Queue, &queue)?);
        }
    }

    // Cron schedules. `wrangler triggers deploy` replaces the Worker's
    // schedules only when `[triggers] crons` is present, so the rendered file
    // always carries it: an application that drops its last cron gets an
    // empty list, which clears the schedule.
    let source_crons = match doc
        .get("triggers")
        .and_then(|triggers| triggers.get("crons"))
    {
        None => None,
        Some(item) => Some(
            item.as_array()
                .and_then(|array| {
                    array
                        .iter()
                        .map(|cron| cron.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| ArtifactError::Invalid {
                    message: "[triggers] crons is not a list of strings".to_owned(),
                })?,
        ),
    };
    if source_crons
        .as_ref()
        .is_some_and(|crons| crons != &plan.crons)
        || (source_crons.is_none() && !plan.crons.is_empty())
    {
        return Err(ArtifactError::Invalid {
            message: format!(
                "[triggers] crons in {WRANGLER_TOML_FILE} do not match binding-plan.json's {:?}",
                plan.crons
            ),
        });
    }
    let mut crons = toml_edit::Array::new();
    for cron in &plan.crons {
        crons.push(cron.as_str());
    }
    if !doc.contains_key("triggers") {
        doc["triggers"] = Item::Table(toml_edit::Table::new());
    }
    doc["triggers"]["crons"] = value(crons);

    if let Some(vars) = doc.get_mut("vars").and_then(Item::as_table_like_mut) {
        for (name, item) in vars.iter_mut() {
            let Some(current) = item.as_str() else {
                continue;
            };
            if let Some(config_name) = current.strip_prefix(UNRESOLVED_CONFIG_PREFIX) {
                let resolved =
                    inputs
                        .config
                        .get(config_name)
                        .ok_or_else(|| ArtifactError::Unresolved {
                            placeholder: current.to_owned(),
                        })?;
                if config_name != name.get() {
                    return Err(ArtifactError::Invalid {
                        message: format!(
                            "[vars] {} carries the placeholder for {config_name:?}",
                            name.get()
                        ),
                    });
                }
                *item = value(resolved.as_str());
            }
        }
    }

    // Uploaded versions stay private until activate moves traffic to them.
    // Left unset, wrangler lets the API default decide, which follows the
    // workers.dev route that activate enables.
    doc["preview_urls"] = value(false);

    let rendered = doc.to_string();
    assets_directory(&rendered)?;
    if let Some(placeholder) = find_placeholder(&rendered) {
        return Err(ArtifactError::Unresolved { placeholder });
    }
    Ok(rendered)
}

/// A resource section of `wrangler.toml`: its table path, the binding kind,
/// the key naming the binding, and the fields the driver fills.
type Section = (
    &'static [&'static str],
    BindingKind,
    &'static str,
    &'static [(&'static str, Field)],
);

#[derive(Clone, Copy)]
enum Field {
    Resolved,
    Name,
}

fn array_of_tables<'d>(
    doc: &'d mut DocumentMut,
    path: &[&str],
) -> Option<&'d mut toml_edit::ArrayOfTables> {
    let mut item = doc.as_item_mut();
    for key in path {
        item = item.as_table_like_mut()?.get_mut(key)?;
    }
    item.as_array_of_tables_mut()
}

/// The `[assets] directory` a rendered `wrangler.toml` names, relative to
/// the file, if it has one. Only a plain relative path inside the artifact
/// is accepted.
pub fn assets_directory(rendered: &str) -> Result<Option<PathBuf>, ArtifactError> {
    let doc: DocumentMut = rendered.parse().map_err(|error| ArtifactError::Invalid {
        message: format!("{WRANGLER_TOML_FILE} is not valid TOML: {error}"),
    })?;
    let Some(assets) = doc.get("assets") else {
        return Ok(None);
    };
    let directory = assets
        .get("directory")
        .and_then(Item::as_str)
        .ok_or_else(|| ArtifactError::Invalid {
            message: "[assets] has no directory".to_owned(),
        })?;
    let path = PathBuf::from(directory);
    let inside = path
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
        && path
            .components()
            .any(|component| matches!(component, Component::Normal(_)));
    if !inside {
        return Err(ArtifactError::Invalid {
            message: format!(
                "[assets] directory {directory:?} is not a relative path inside the artifact"
            ),
        });
    }
    Ok(Some(path))
}

/// The first `deis:unresolved/...` token in `text`, if any, outside TOML
/// comments.
pub fn find_placeholder(text: &str) -> Option<String> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .find_map(|line| {
            let start = line.find(UNRESOLVED_RESOURCE_REF_PREFIX)?;
            let token = line[start..]
                .split(|c: char| c == '"' || c == '\'' || c.is_whitespace())
                .next()
                .unwrap_or_default();
            Some(token.to_owned())
        })
}

/// Copy the artifact into `staging` (skipping nothing but the files wrangler
/// writes itself) so the rendered `wrangler.toml` never touches the source.
pub fn stage(artifact_dir: &Path, staging: &Path) -> Result<(), ArtifactError> {
    copy_tree(artifact_dir, staging).map_err(|error| ArtifactError::Missing {
        file: artifact_dir.display().to_string(),
        message: format!("copying the artifact for staging: {error}"),
    })
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".wrangler" || name == "node_modules" {
            continue;
        }
        let file_type = entry.file_type()?;
        let target = to.join(&name);
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[derive(Debug)]
pub enum ArtifactError {
    Missing {
        file: String,
        message: String,
    },
    Invalid {
        message: String,
    },
    /// A placeholder has no value to be replaced with.
    Unresolved {
        placeholder: String,
    },
}

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { file, message } => write!(f, "{file}: {message}"),
            Self::Invalid { message } => write!(f, "{message}"),
            Self::Unresolved { placeholder } => write!(
                f,
                "{placeholder} has no resolved value; the driver refuses to deploy a placeholder"
            ),
        }
    }
}

impl std::error::Error for ArtifactError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_directory_accepts_only_a_path_inside_the_artifact() {
        assert_eq!(assets_directory("name = \"w\"\n").unwrap(), None);
        assert_eq!(
            assets_directory("[assets]\ndirectory = \"./assets\"\n").unwrap(),
            Some(PathBuf::from("./assets"))
        );
        for directory in ["../elsewhere", "/etc", "./a/../../b", ".", ""] {
            let rendered = format!("[assets]\ndirectory = {directory:?}\n");
            assert!(
                assets_directory(&rendered).is_err(),
                "{directory:?} must be refused"
            );
        }
        assert!(assets_directory("[assets]\nbinding = \"ASSETS\"\n").is_err());
    }
}
