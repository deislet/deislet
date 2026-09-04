use deno_ast::swc::ast::{
    CallExpr, Callee, ExportAll, ExportSpecifier, Expr, ImportDecl, ImportSpecifier, Lit,
    ModuleExportName, NamedExport, Str,
};
use deno_ast::swc::ecma_visit::{Visit, VisitWith};
use deno_ast::{SourceRangedForSpanned, StartSourcePos};
use deno_core::{extension, op2, OpState};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

/// The mutable `process.env` view of the invocation currently executing.
///
/// This is deliberately separate from [`super::ops::EnvStore`], which is the
/// deployment configuration shared by every invocation. Writes through the
/// Node facade affect only this optional snapshot and are discarded at the
/// invocation boundary in `AppRuntime`, including termination paths.
pub type NodeProcessEnvStore = Rc<RefCell<Option<HashMap<String, String>>>>;

#[derive(Default)]
struct NodeImportVisitor {
    references: Vec<NodeReference>,
}

struct NodeReference {
    specifier: String,
    exports: Vec<String>,
    namespace: bool,
    start: usize,
    end: usize,
}

impl NodeImportVisitor {
    fn push(&mut self, source: &Str, exports: Vec<String>, namespace: bool) {
        self.references.push(NodeReference {
            specifier: source.value.to_string_lossy().into_owned(),
            exports,
            namespace,
            start: SourceRangedForSpanned::start(source)
                .as_byte_index(StartSourcePos::START_SOURCE_POS),
            end: SourceRangedForSpanned::end(source)
                .as_byte_index(StartSourcePos::START_SOURCE_POS),
        });
    }
}

fn export_name(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::Ident(ident) => ident.sym.to_string(),
        ModuleExportName::Str(value) => value.value.to_string_lossy().into_owned(),
    }
}

impl Visit for NodeImportVisitor {
    fn visit_import_decl(&mut self, declaration: &ImportDecl) {
        if declaration.type_only {
            return;
        }
        let mut exports = Vec::new();
        let mut namespace = false;
        for specifier in &declaration.specifiers {
            match specifier {
                ImportSpecifier::Named(named) if !named.is_type_only => exports.push(
                    named
                        .imported
                        .as_ref()
                        .map(export_name)
                        .unwrap_or_else(|| named.local.sym.to_string()),
                ),
                ImportSpecifier::Default(_) => exports.push("default".to_string()),
                ImportSpecifier::Namespace(_) => namespace = true,
                ImportSpecifier::Named(_) => {}
            }
        }
        self.push(&declaration.src, exports, namespace);
    }

    fn visit_named_export(&mut self, declaration: &NamedExport) {
        let Some(source) = &declaration.src else {
            return;
        };
        if declaration.type_only {
            return;
        }
        let mut exports = Vec::new();
        let mut namespace = false;
        for specifier in &declaration.specifiers {
            match specifier {
                ExportSpecifier::Named(named) if !named.is_type_only => {
                    exports.push(export_name(&named.orig));
                }
                ExportSpecifier::Default(_) => exports.push("default".to_string()),
                ExportSpecifier::Namespace(_) => namespace = true,
                ExportSpecifier::Named(_) => {}
            }
        }
        self.push(source, exports, namespace);
    }

    fn visit_export_all(&mut self, declaration: &ExportAll) {
        if !declaration.type_only {
            self.push(&declaration.src, Vec::new(), true);
        }
    }

    fn visit_call_expr(&mut self, call: &CallExpr) {
        if matches!(call.callee, Callee::Import(_)) {
            if let Some(argument) = call.args.first() {
                if let Expr::Lit(Lit::Str(source)) = argument.expr.as_ref() {
                    self.push(source, Vec::new(), true);
                }
            }
        }
        call.visit_children_with(self);
    }
}

/// Validate the runtime-visible Node edges of the single deployed artifact.
///
/// The compiler performs the authoritative source and package validation.
/// This second gate is for a bundle that bypassed it or was changed after it:
/// deno_core resolves registered `node:` modules before consulting the custom
/// loader, so checking only `ModuleLoader::resolve` cannot be a security
/// boundary.
fn validated_node_references(
    parsed: &deno_ast::ParsedSource,
) -> Result<Vec<NodeReference>, String> {
    let mut visitor = NodeImportVisitor::default();
    parsed.program_ref().visit_with(&mut visitor);

    for reference in &visitor.references {
        let Some(canonical) = deis_node_contract::canonical_builtin(&reference.specifier) else {
            if deis_node_contract::is_known_node_builtin(&reference.specifier)
                || reference.specifier.starts_with("node:")
            {
                return Err(format!(
                    "EC_NODE_ARTIFACT_INCOMPLETE: {} is outside the Edge Canon Node {} surface",
                    reference.specifier,
                    deis_node_contract::NODE_SEMANTIC_BASELINE
                ));
            }
            continue;
        };
        let allowed = deis_node_contract::builtin_exports(canonical)
            .expect("every canonical builtin has an export list");
        if reference.namespace {
            return Err(format!(
                "EC_NODE_ARTIFACT_INCOMPLETE: {} uses a namespace-producing import",
                reference.specifier
            ));
        }
        if let Some(name) = reference
            .exports
            .iter()
            .find(|name| !allowed.contains(&name.as_str()))
        {
            return Err(format!(
                "EC_NODE_ARTIFACT_INCOMPLETE: {canonical} export {name:?} is outside the selected surface"
            ));
        }
    }
    Ok(visitor.references)
}

/// Rewrite approved application imports to a private module scheme.
///
/// deno_core dispatches registered synthetic `node:` modules by the raw
/// source specifier before it consults the embedder's module loader. The
/// validation above is therefore paired with a source rewrite: application
/// imports reach only the exact Edge Canon facade, while deno_node's own
/// internal imports keep their native `node:` identity.
pub fn rewrite_node_artifact(parsed: &deno_ast::ParsedSource) -> Result<String, String> {
    let references = validated_node_references(parsed)?;
    let mut replacements = references
        .into_iter()
        .filter_map(|reference| {
            deis_node_contract::canonical_builtin(&reference.specifier).map(|canonical| {
                (
                    reference.start,
                    reference.end,
                    serde_json::to_string(&runtime_builtin_specifier(canonical))
                        .expect("an internal module URL is JSON-serializable"),
                )
            })
        })
        .collect::<Vec<_>>();
    replacements.sort_unstable_by_key(|(start, _, _)| std::cmp::Reverse(*start));

    let mut source = parsed.text().to_string();
    for (start, end, replacement) in replacements {
        source.replace_range(start..end, &replacement);
    }
    Ok(source)
}

pub fn runtime_builtin_specifier(canonical: &str) -> String {
    format!(
        "deis-node:///builtin/{}.js",
        canonical.trim_start_matches("node:")
    )
}

// deno_node's internal performance bookkeeping imports this worker-host op
// while loading the selected stream/zlib modules. Deislet has no JavaScript
// worker threads, so there is no separately measurable worker CPU clock; a
// stable zero pair is the only truthful value and avoids registering Deno's
// general worker-creation surface merely to satisfy the internal import.
#[op2(fast)]
fn op_current_thread_cpu_usage(#[buffer] out: &mut [f64]) {
    if out.len() >= 2 {
        out[0] = 0.0;
        out[1] = 0.0;
    }
}

#[op2]
#[string]
fn op_deis_node_env_get(state: &mut OpState, #[string] key: String) -> Option<String> {
    state
        .borrow::<NodeProcessEnvStore>()
        .borrow()
        .as_ref()
        .and_then(|env| env.get(&key).cloned())
}

#[op2]
#[serde]
fn op_deis_node_env_keys(state: &mut OpState) -> Vec<String> {
    let mut keys: Vec<String> = state
        .borrow::<NodeProcessEnvStore>()
        .borrow()
        .as_ref()
        .map(|env| env.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort_unstable();
    keys
}

#[op2]
#[serde]
fn op_deis_node_env_snapshot(state: &mut OpState) -> BTreeMap<String, String> {
    state
        .borrow::<NodeProcessEnvStore>()
        .borrow()
        .as_ref()
        .map(|env| {
            env.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Return the selected surface from the same crate used by the compiler.
///
/// JavaScript deliberately does not keep a second hand-written inventory:
/// one evolving contract must govern source validation, artifact validation,
/// and the objects an application can obtain at runtime.
#[op2]
#[serde]
fn op_deis_node_contract() -> BTreeMap<String, Vec<String>> {
    deis_node_contract::NODE_BUILTIN_MODULES
        .iter()
        .map(|specifier| {
            (
                (*specifier).to_string(),
                deis_node_contract::builtin_exports(specifier)
                    .expect("every selected Node module has an export list")
                    .iter()
                    .map(|name| (*name).to_string())
                    .collect(),
            )
        })
        .collect()
}

#[op2]
#[string]
fn op_deis_node_version() -> &'static str {
    deis_node_contract::NODE_SEMANTIC_BASELINE
}

#[op2(fast)]
fn op_deis_node_env_set(
    state: &mut OpState,
    #[string] key: String,
    #[string] value: String,
) -> bool {
    let store = state.borrow::<NodeProcessEnvStore>();
    let mut current = store.borrow_mut();
    let Some(env) = current.as_mut() else {
        return false;
    };
    env.insert(key, value);
    true
}

#[op2(fast)]
fn op_deis_node_env_delete(state: &mut OpState, #[string] key: String) -> bool {
    let store = state.borrow::<NodeProcessEnvStore>();
    store
        .borrow_mut()
        .as_mut()
        .map(|env| env.remove(&key).is_some())
        .unwrap_or(false)
}

extension!(
    deis_node_compat,
    ops = [
        op_deis_node_env_get,
        op_deis_node_env_keys,
        op_deis_node_env_snapshot,
        op_deis_node_env_set,
        op_deis_node_env_delete,
        op_deis_node_contract,
        op_deis_node_version,
        op_current_thread_cpu_usage,
    ],
);

pub fn init_node_compat_extension() -> deno_core::Extension {
    deis_node_compat::init()
}

#[cfg(test)]
mod tests {
    use super::*;
    use deno_ast::{MediaType, ParseParams};
    use deno_core::ModuleSpecifier;

    fn parse(source: &str) -> deno_ast::ParsedSource {
        deno_ast::parse_module(ParseParams {
            specifier: ModuleSpecifier::parse("deis:///apps/rewrite.production/app.ts").unwrap(),
            text: source.to_string().into(),
            media_type: MediaType::TypeScript,
            capture_tokens: false,
            scope_analysis: false,
            maybe_syntax: None,
        })
        .unwrap()
    }

    #[test]
    fn approved_node_edges_are_rewritten_without_touching_other_text() {
        let source = r#"
            const marker = "例: node:path";
            import { resolve } from 'node:path';
            import { Buffer } from "buffer";
            export { domainToASCII as ascii } from "node:url";
        "#;
        let rewritten = rewrite_node_artifact(&parse(source)).unwrap();

        assert!(rewritten.contains(r#"const marker = "例: node:path""#));
        assert!(rewritten.contains(r#"from "deis-node:///builtin/path.js""#));
        assert!(rewritten.contains(r#"from "deis-node:///builtin/buffer.js""#));
        assert!(rewritten.contains(r#"from "deis-node:///builtin/url.js""#));
        assert!(!rewritten.contains("from 'node:path'"));
    }

    #[test]
    fn a_node_edge_that_could_expand_the_surface_is_rejected_before_rewrite() {
        for (source, detail) in [
            (
                r#"import * as path from "node:path";"#,
                "namespace-producing",
            ),
            (r#"import { readFile } from "node:fs";"#, "node:fs"),
            (r#"await import("node:path");"#, "namespace-producing"),
        ] {
            let error = rewrite_node_artifact(&parse(source)).unwrap_err();
            assert!(error.contains(detail), "{error}");
        }
    }
}
