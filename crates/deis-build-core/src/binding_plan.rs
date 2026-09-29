//! The standard binding plan: what a project's logical resources are, before
//! any provider is asked to host them.
//!
//! A project declares logical resources by name - `"HOT": { "kind": "kv" }` -
//! and a provider target turns each of them into something its own platform
//! hosts. What the plan records is exactly the tuple the requirement fixes:
//! the logical `kind`, the `name` the application reaches it by, and a
//! `provider_resource_ref` that names the provider-side resource *without
//! ever inventing one*.
//!
//! That last field is the part this module exists to keep honest. A build
//! cannot know which KV namespace a deployment will use: resources are
//! created or imported by the provider driver from the deployment state, at
//! prepare time, after the build. So `provider_resource_ref` here is a
//! stable, machine-findable **placeholder** - `deis:unresolved/<kind>/<name>`
//! - and the contract is:
//!
//! * the build emits the placeholder verbatim into every provider artifact
//!   (the Cloudflare adapter writes it into `wrangler.toml` and
//!   `binding-plan.json`);
//! * the driver replaces every occurrence of the token with the real
//!   resource identifier it resolved from the deployment state, before any
//!   provider tool runs;
//! * nothing downstream ever sees an empty identifier or a made-up one. An
//!   artifact that still carries `deis:unresolved/` after prepare is invalid,
//!   and the driver refuses to deploy it.
//!
//! The plan is deliberately provider-neutral about everything except the
//! placeholder spelling: which provider sections a kind becomes is the
//! adapter's decision, not this module's.

use serde::{Deserialize, Serialize};

use crate::config::ProjectConfig;

/// The document format `binding-plan.json` carries.
pub const BINDING_PLAN_FORMAT: &str = "deislet.binding-plan/v1";

/// The target this plan's placeholder mechanism is specified against.
///
/// Other provider targets reuse [`build_binding_plan`] only where they can
/// honour every mapping; the plan itself records which target it was built
/// for so a driver cannot consume one built for a different vendor.
pub const BINDING_PLAN_TARGET_CLOUDFLARE: &str = "cloudflare";

/// The prefix of every unresolved resource reference a build emits.
///
/// Kept here rather than in the adapter, because the driver, the adapter and
/// this planner all have to agree on the spelling, and a token whose halves
/// drifted apart would resolve nothing.
pub const UNRESOLVED_RESOURCE_REF_PREFIX: &str = "deis:unresolved";

/// The retries a queue consumer is held to when the project does not declare
/// a number.
///
/// The standard requires the value to be *declared*, so every plan states one
/// explicitly: this number is the standard's default, and the deislet queue
/// service applies the same one. It matches Cloudflare Queues' own default
/// (`max_retries` of 3), so a consumer that says nothing behaves the same on
/// both targets.
pub const DEFAULT_QUEUE_MAX_RETRIES: u32 = 3;

/// One logical resource and the provider resource it becomes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedBinding {
    /// The logical kind: `kv`, `database`, `blob` or `queue`.
    pub kind: BindingKind,
    /// The name the application reaches the resource by. It is also the
    /// provider-side binding name: on Cloudflare, `env.<name>` is the object
    /// `wrangler.toml` binds.
    pub name: String,
    /// The unresolved placeholder this build emits for the provider-side
    /// resource identifier. See the module documentation for the fill-in
    /// contract.
    pub provider_resource_ref: String,
}

/// The standard kinds a logical binding can name, restricted to the ones this
/// plan maps onto a provider resource.
///
/// `service` is deliberately absent: it is a route to another *application*,
/// not a resource, and no provider mapping for it is planned (the Cloudflare
/// target refuses it - see [`build_binding_plan`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingKind {
    Kv,
    Database,
    Blob,
    Queue,
}

impl BindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kv => "kv",
            Self::Database => "database",
            Self::Blob => "blob",
            Self::Queue => "queue",
        }
    }

    /// The kind a project-file spelling names, or `None` for one this plan
    /// does not map (including `service` and anything newer).
    pub fn from_project_kind(kind: &str) -> Option<Self> {
        match kind {
            "kv" => Some(Self::Kv),
            "database" => Some(Self::Database),
            "blob" => Some(Self::Blob),
            "queue" => Some(Self::Queue),
            _ => None,
        }
    }
}

/// One queue the application consumes from, as the plan states it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedQueueConsumer {
    /// The queue's logical name. It is both the application's name for it and
    /// the provider-side queue name the driver provisions.
    pub queue: String,
    /// How many messages one delivery carries. Carried verbatim from the
    /// project file; the provider decides what an absent value means.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_batch_size: Option<u32>,
    /// How many deliveries may be in flight at once. Same rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<u32>,
    /// How many times a failing delivery is retried before the message is
    /// dead-lettered. Always stated: the standard requires the value to be
    /// explicit, and [`DEFAULT_QUEUE_MAX_RETRIES`] fills in an absent one.
    pub max_retries: u32,
}

/// The provider-neutral plan for one project's logical resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingPlan {
    /// The document format. A reader that does not know it refuses the file
    /// rather than guessing at the fields.
    pub format: &'static str,
    /// The provider target this plan was built for. A driver serving another
    /// target refuses it.
    pub target: &'static str,
    /// The mapped logical bindings, sorted by name.
    pub bindings: Vec<PlannedBinding>,
    /// The queues the application consumes from, sorted by name.
    pub queue_consumers: Vec<PlannedQueueConsumer>,
    /// The cron expressions the application is triggered on, verbatim from
    /// the project file, in the order it wrote them.
    pub crons: Vec<String>,
}

/// The unresolved placeholder for one logical resource.
pub fn unresolved_resource_ref(kind: BindingKind, name: &str) -> String {
    format!("{UNRESOLVED_RESOURCE_REF_PREFIX}/{}/{name}", kind.as_str())
}

/// Whether a resource reference is still an unresolved placeholder.
pub fn is_unresolved_resource_ref(reference: &str) -> bool {
    reference.starts_with(UNRESOLVED_RESOURCE_REF_PREFIX)
}

/// A name usable as a provider-side binding.
///
/// The provider target publishes each binding as a property on the runtime's
/// environment object, so the name has to be spellable there: ASCII letters,
/// digits and underscores, not starting with a digit. The project file's own
/// rules are wider (a `-` is legal because it lands in a filesystem path on
/// the self-hosted target), so this is a provider-mapping rule, checked where
/// the mapping is made.
fn is_provider_binding_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Build the binding plan for the Cloudflare target from a project file.
///
/// Every logical resource the project declares must be mappable, or the whole
/// plan fails: a plan that silently dropped one binding would build an
/// application that reads `env.MISSING` at runtime. What is refused, and why:
///
/// * **`service` bindings** - a service binding calls another *application*,
///   and no Cloudflare product carries that semantics. Until a mapping is
///   proposed and accepted, this is unimplemented, not optional.
/// * **`callableBy`** - the consenting half of the same feature, refused for
///   the same reason.
///
/// The errors use `EC_BINDING_TARGET_UNIMPLEMENTED`, because each of them
/// means "this target does not implement the capability the project asks
/// for" - the same refusal the compiler has always raised, now answered from
/// the one place that knows precisely what the target does map.
pub fn build_binding_plan(config: &ProjectConfig) -> anyhow::Result<BindingPlan> {
    build_binding_plan_for_target(config, BINDING_PLAN_TARGET_CLOUDFLARE)
}

/// [`build_binding_plan`] for an explicit target name, so a future provider
/// adapter does not have to lie about whose plan it builds. The name is
/// carried into the plan verbatim, so it is a `'static` literal: a plan that
/// named its target with a temporary string could not be serialised
/// round-tripped safely.
pub fn build_binding_plan_for_target(
    config: &ProjectConfig,
    target: &'static str,
) -> anyhow::Result<BindingPlan> {
    // Sorted, so the plan is a pure function of the project file and the
    // same project always plans identically. The project file's bindings are
    // a JSON object read into a HashMap, so iteration order is address
    // order, not declaration order - sorting is what makes two builds of one
    // project produce byte-identical plans and provider configs.
    let mut bindings = Vec::new();
    let mut declared = config.bindings.iter().collect::<Vec<_>>();
    declared.sort_by(|left, right| left.0.cmp(right.0));
    for (name, binding) in declared {
        let Some(kind) = BindingKind::from_project_kind(&binding.kind) else {
            if binding.kind == "service" {
                anyhow::bail!(
                    "EC_BINDING_TARGET_UNIMPLEMENTED: the {target} target has no mapping for the \
                     service binding {name:?}; calling another application is a platform \
                     capability, and no {target} product carries it. An Edge Canon proposal for \
                     one is the path to a mapping - until then the binding is refused, not \
                     degraded"
                );
            }
            anyhow::bail!(
                "EC_BINDING_INVALID_KIND: binding {name:?} declares the kind {:?}, which the \
                 standard does not define",
                binding.kind
            );
        };
        if !is_provider_binding_name(name) {
            anyhow::bail!(
                "EC_BINDING_TARGET_UNIMPLEMENTED: the {target} target cannot map the binding name \
                 {name:?}. A binding name becomes a property on the provider's environment \
                 object, which ASCII letters, digits and '_' can spell; the self-hosted target's \
                 wider name rule accepts '-' for its own storage paths, and no spelling of this \
                 name exists on {target}"
            );
        }
        bindings.push(PlannedBinding {
            kind,
            name: name.clone(),
            provider_resource_ref: unresolved_resource_ref(kind, name),
        });
    }

    if !config.callable_by.is_empty() {
        anyhow::bail!(
            "EC_BINDING_TARGET_UNIMPLEMENTED: the {target} target has no mapping for \
             `callableBy`; admitting calls from other applications is a platform capability, \
             and no {target} product carries it"
        );
    }

    let mut queue_consumers = config
        .queues
        .iter()
        .map(|queue| PlannedQueueConsumer {
            queue: queue.name.clone(),
            max_batch_size: queue.batch_size,
            max_concurrency: queue.max_concurrency,
            max_retries: queue.max_retries.unwrap_or(DEFAULT_QUEUE_MAX_RETRIES),
        })
        .collect::<Vec<_>>();
    queue_consumers.sort_by(|left, right| left.queue.cmp(&right.queue));

    Ok(BindingPlan {
        format: BINDING_PLAN_FORMAT,
        target,
        bindings,
        queue_consumers,
        crons: config.cron.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProjectConfig, QueueConsumerConfig};

    fn config_with(bindings: serde_json::Value) -> ProjectConfig {
        let mut value = serde_json::json!({
            "version": "1.0",
            "name": "planned",
            "runtime": "standard-v1",
            "language": "typescript",
        });
        value["bindings"] = bindings;
        serde_json::from_value(value).expect("config parses")
    }

    #[test]
    fn every_storage_kind_plans_with_a_placeholder_ref() {
        let config = config_with(serde_json::json!({
            "HOT": {"kind": "kv"},
            "DB": {"kind": "database"},
            "FILES": {"kind": "blob"},
            "ORDERS": {"kind": "queue"}
        }));
        let plan = build_binding_plan(&config).expect("all four kinds map");
        assert_eq!(plan.target, "cloudflare");
        assert_eq!(plan.format, BINDING_PLAN_FORMAT);
        let names: Vec<&str> = plan.bindings.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["DB", "FILES", "HOT", "ORDERS"],
            "sorted by name"
        );
        for binding in &plan.bindings {
            assert_eq!(
                binding.provider_resource_ref,
                format!("deis:unresolved/{}/{}", binding.kind.as_str(), binding.name)
            );
            assert!(is_unresolved_resource_ref(&binding.provider_resource_ref));
        }
        assert_eq!(plan.bindings[0].kind, BindingKind::Database);
        assert_eq!(plan.bindings[3].kind, BindingKind::Queue);
    }

    #[test]
    fn a_service_binding_is_refused_as_unimplemented_not_dropped() {
        let config =
            config_with(serde_json::json!({"AUTH": {"kind": "service", "app": "auth-api"}}));
        let error = build_binding_plan(&config).unwrap_err().to_string();
        assert!(error.contains("EC_BINDING_TARGET_UNIMPLEMENTED"), "{error}");
        assert!(error.contains("AUTH"), "{error}");
        assert!(error.contains("service binding"), "{error}");
    }

    #[test]
    fn callable_by_is_refused() {
        let mut config = config_with(serde_json::json!({}));
        config.callable_by = vec!["*".to_string()];
        let error = build_binding_plan(&config).unwrap_err().to_string();
        assert!(error.contains("EC_BINDING_TARGET_UNIMPLEMENTED"), "{error}");
        assert!(error.contains("callableBy"), "{error}");
    }

    #[test]
    fn a_name_the_provider_environment_cannot_spell_is_refused() {
        let config = config_with(serde_json::json!({"MY-KV": {"kind": "kv"}}));
        let error = build_binding_plan(&config).unwrap_err().to_string();
        assert!(error.contains("EC_BINDING_TARGET_UNIMPLEMENTED"), "{error}");
        assert!(error.contains("MY-KV"), "{error}");
    }

    #[test]
    fn an_unknown_kind_is_a_plan_error_not_a_silent_drop() {
        let config = config_with(serde_json::json!({"X": {"kind": "vector"}}));
        let error = build_binding_plan(&config).unwrap_err().to_string();
        assert!(error.contains("EC_BINDING_INVALID_KIND"), "{error}");
    }

    #[test]
    fn queue_consumers_state_retries_explicitly_with_the_standard_default() {
        let mut config = config_with(serde_json::json!({}));
        config.queues = vec![
            QueueConsumerConfig {
                name: "jobs".to_string(),
                batch_size: Some(10),
                max_concurrency: None,
                max_retries: None,
            },
            QueueConsumerConfig {
                name: "mail".to_string(),
                batch_size: None,
                max_concurrency: Some(2),
                max_retries: Some(1),
            },
        ];
        let plan = build_binding_plan(&config).expect("consumers plan");
        assert_eq!(
            plan.queue_consumers,
            vec![
                PlannedQueueConsumer {
                    queue: "jobs".to_string(),
                    max_batch_size: Some(10),
                    max_concurrency: None,
                    max_retries: DEFAULT_QUEUE_MAX_RETRIES,
                },
                PlannedQueueConsumer {
                    queue: "mail".to_string(),
                    max_batch_size: None,
                    max_concurrency: Some(2),
                    max_retries: 1,
                },
            ]
        );
    }

    #[test]
    fn crons_are_carried_verbatim() {
        let mut config = config_with(serde_json::json!({}));
        config.cron = vec!["0 * * * *".to_string(), "*/5 * * * *".to_string()];
        let plan = build_binding_plan(&config).expect("crons plan");
        assert_eq!(plan.crons, vec!["0 * * * *", "*/5 * * * *"]);
    }

    #[test]
    fn the_plan_serialises_to_the_document_the_driver_reads() {
        let config = config_with(serde_json::json!({"HOT": {"kind": "kv"}}));
        let plan = build_binding_plan(&config).expect("plan");
        let json = serde_json::to_value(&plan).expect("serialise");
        assert_eq!(json["format"], BINDING_PLAN_FORMAT);
        assert_eq!(json["target"], "cloudflare");
        assert_eq!(json["bindings"][0]["kind"], "kv");
        assert_eq!(json["bindings"][0]["name"], "HOT");
        assert_eq!(
            json["bindings"][0]["provider_resource_ref"],
            "deis:unresolved/kv/HOT"
        );
    }

    #[test]
    fn an_empty_project_plans_to_an_empty_document() {
        let config = config_with(serde_json::json!({}));
        let plan = build_binding_plan(&config).expect("nothing declared, nothing mapped");
        assert!(plan.bindings.is_empty());
        assert!(plan.queue_consumers.is_empty());
        assert!(plan.crons.is_empty());
    }
}
