use crate::app_repo::AppRepo;
use crate::runtime::node_compat::{rewrite_node_artifact, runtime_builtin_specifier};
use anyhow::Result;
use deno_ast::{MediaType, ParseParams};
use deno_core::{
    ModuleLoadOptions, ModuleLoadResponse, ModuleLoader, ModuleSource, ModuleSourceCode,
    ModuleSpecifier, ModuleType, ResolutionKind,
};
use deno_error::JsErrorBox;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

const BUILTIN_MODULE_PREFIX: &str = "deis-node:///builtin/";

fn resolve_application_node_module(
    specifier: &str,
    referrer: &str,
) -> Result<Option<ModuleSpecifier>, JsErrorBox> {
    if !referrer.starts_with("deis:") {
        return Ok(None);
    }

    if let Some(canonical) = deis_node_contract::canonical_builtin(specifier) {
        return ModuleSpecifier::parse(&runtime_builtin_specifier(canonical))
            .map(Some)
            .map_err(JsErrorBox::from_err);
    }

    if deis_node_contract::is_known_node_builtin(specifier) || specifier.starts_with("node:") {
        return Err(JsErrorBox::generic(format!(
            "EC_NODE_ARTIFACT_INCOMPLETE: {specifier} is not in the Edge Canon Node {} runtime surface",
            deis_node_contract::NODE_SEMANTIC_BASELINE
        )));
    }

    Ok(None)
}

fn selected_builtin_source(specifier: &ModuleSpecifier) -> Option<String> {
    let relative = specifier.as_str().strip_prefix(BUILTIN_MODULE_PREFIX)?;
    let bare = relative.strip_suffix(".js")?;
    let canonical = deis_node_contract::canonical_builtin(bare)?;
    let expected = format!(
        "{BUILTIN_MODULE_PREFIX}{}.js",
        canonical.trim_start_matches("node:")
    );
    if specifier.as_str() != expected {
        return None;
    }

    let mut source = format!(
        "const selected = globalThis.process.getBuiltinModule({canonical:?});\nif (selected === undefined) throw new Error(\"EC_NODE_ARTIFACT_INCOMPLETE: selected builtin is unavailable\");\n"
    );
    for name in
        deis_node_contract::builtin_exports(canonical).expect("every selected builtin has exports")
    {
        source.push_str(&format!("export const {name} = selected.{name};\n"));
    }
    Some(source)
}

fn embedded_node_module(specifier: &ModuleSpecifier) -> Option<Vec<u8>> {
    selected_builtin_source(specifier).map(String::into_bytes)
}

fn is_executable_source(media_type: MediaType) -> bool {
    matches!(
        media_type,
        MediaType::JavaScript
            | MediaType::Jsx
            | MediaType::Mjs
            | MediaType::Cjs
            | MediaType::TypeScript
            | MediaType::Mts
            | MediaType::Cts
            | MediaType::Tsx
            | MediaType::Unknown
    )
}

fn prepare_module_source(
    module_specifier: &ModuleSpecifier,
    bytes: Vec<u8>,
) -> Result<(Vec<u8>, ModuleType), JsErrorBox> {
    let media_type = MediaType::from_specifier(module_specifier);
    if media_type == MediaType::Wasm {
        return Ok((bytes, ModuleType::Wasm));
    }

    if module_specifier.scheme() != "deis" || !is_executable_source(media_type) {
        return Ok((bytes, ModuleType::JavaScript));
    }

    let text = String::from_utf8(bytes).map_err(|error| JsErrorBox::generic(error.to_string()))?;
    let parsed = deno_ast::parse_module(ParseParams {
        specifier: module_specifier.clone(),
        text: text.into(),
        media_type,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(JsErrorBox::from_err)?;
    let rewritten = rewrite_node_artifact(&parsed).map_err(JsErrorBox::generic)?;

    if !media_type.is_emittable() {
        return Ok((rewritten.into_bytes(), ModuleType::JavaScript));
    }

    // Reparse after replacing module string literals so the emitted program,
    // spans and source text all describe the same input.
    let parsed = deno_ast::parse_module(ParseParams {
        specifier: module_specifier.clone(),
        text: rewritten.into(),
        media_type,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(JsErrorBox::from_err)?;
    let transpiled = parsed
        .transpile(
            &Default::default(),
            &Default::default(),
            &Default::default(),
        )
        .map_err(JsErrorBox::from_err)?;
    Ok((
        transpiled.into_source().text.into_bytes(),
        ModuleType::JavaScript,
    ))
}

#[cfg(test)]
mod node_facade_tests {
    use super::*;

    #[test]
    fn selected_builtins_have_facades_with_only_their_locked_exports() {
        for canonical in deis_node_contract::NODE_BUILTIN_MODULES {
            let url = ModuleSpecifier::parse(&runtime_builtin_specifier(canonical)).unwrap();
            let source = String::from_utf8(embedded_node_module(&url).unwrap()).unwrap();
            for name in deis_node_contract::builtin_exports(canonical).unwrap() {
                assert!(source.contains(&format!("export const {name} = selected.{name};")));
            }
            assert_eq!(
                source.matches("export const ").count(),
                deis_node_contract::builtin_exports(canonical)
                    .unwrap()
                    .len()
            );
        }
    }

    #[test]
    fn every_application_module_passes_the_node_artifact_backstop() {
        let specifier =
            ModuleSpecifier::parse("deis:///apps/multi-file.production/internal.js").unwrap();
        let error = prepare_module_source(
            &specifier,
            br#"import { readFile } from "node:fs"; export { readFile };"#.to_vec(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("EC_NODE_ARTIFACT_INCOMPLETE"));
        assert!(error.to_string().contains("node:fs"));
    }
}

#[derive(Clone)]
pub struct MemoryModuleLoader {
    modules: Arc<HashMap<ModuleSpecifier, Vec<u8>>>,
}

impl MemoryModuleLoader {
    pub fn new(modules: HashMap<ModuleSpecifier, Vec<u8>>) -> Self {
        Self {
            modules: Arc::new(modules),
        }
    }

    pub fn with_main_module(specifier: &str, code: String) -> Result<Self> {
        let mut modules = HashMap::new();
        let url = ModuleSpecifier::parse(specifier)?;
        modules.insert(url, code.into_bytes());
        Ok(Self::new(modules))
    }
}

#[derive(Clone)]
pub struct DbModuleLoader {
    app_id: String,
    app_repo: Option<Arc<AppRepo>>,
    modules: Arc<RwLock<HashMap<ModuleSpecifier, Vec<u8>>>>,
    path_prefix: String,
}

impl DbModuleLoader {
    pub fn new(
        app_id: String,
        entry_specifier: ModuleSpecifier,
        entry_code: Vec<u8>,
        app_repo: Option<Arc<AppRepo>>,
    ) -> Result<Self> {
        let mut modules = HashMap::new();
        modules.insert(entry_specifier, entry_code);
        let path_prefix = format!("/apps/{}/", app_id);
        Ok(Self {
            app_id,
            app_repo,
            modules: Arc::new(RwLock::new(modules)),
            path_prefix,
        })
    }

    fn module_path(&self, specifier: &ModuleSpecifier) -> Option<String> {
        if specifier.scheme() != "deis" {
            return None;
        }
        let path = specifier.path();
        if !path.starts_with(&self.path_prefix) {
            return None;
        }
        let relative = path.trim_start_matches(&self.path_prefix);
        let trimmed = relative.trim_start_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        Some(trimmed.to_string())
    }
}

impl ModuleLoader for MemoryModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, JsErrorBox> {
        if let Some(resolved) = resolve_application_node_module(specifier, referrer)? {
            return Ok(resolved);
        }
        deno_core::resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&deno_core::ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let module_specifier = module_specifier.clone();
        let bytes_opt = embedded_node_module(&module_specifier)
            .or_else(|| self.modules.get(&module_specifier).cloned());

        ModuleLoadResponse::Async(Box::pin(async move {
            let bytes = bytes_opt.ok_or_else(|| {
                JsErrorBox::generic(format!("Module not found: {}", module_specifier))
            })?;

            let (source, source_type) = prepare_module_source(&module_specifier, bytes)?;

            Ok(ModuleSource::new(
                source_type,
                ModuleSourceCode::Bytes(source.into_boxed_slice().into()),
                &module_specifier,
                None,
            ))
        }))
    }
}

impl ModuleLoader for DbModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, JsErrorBox> {
        if let Some(resolved) = resolve_application_node_module(specifier, referrer)? {
            return Ok(resolved);
        }
        deno_core::resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&deno_core::ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let module_specifier = module_specifier.clone();
        let modules = self.modules.clone();
        let app_repo = self.app_repo.clone();
        let app_id = self.app_id.clone();
        let embedded = embedded_node_module(&module_specifier);
        let maybe_path = self.module_path(&module_specifier);

        ModuleLoadResponse::Async(Box::pin(async move {
            let cached = if embedded.is_some() {
                embedded
            } else {
                modules.read().await.get(&module_specifier).cloned()
            };
            let bytes = if let Some(b) = cached {
                b
            } else {
                let repo = app_repo.ok_or_else(|| {
                    JsErrorBox::generic(format!("Module not found: {}", module_specifier))
                })?;
                let file_path = maybe_path.ok_or_else(|| {
                    JsErrorBox::generic(format!("Invalid module specifier: {}", module_specifier))
                })?;
                let content = repo
                    .get_file(&app_id, &file_path)
                    .await
                    .map_err(|e| JsErrorBox::generic(e.to_string()))?
                    .ok_or_else(|| {
                        JsErrorBox::generic(format!("Module not found: {}", module_specifier))
                    })?;
                // No longer forcing conversion to String, keeping binary data
                modules
                    .write()
                    .await
                    .insert(module_specifier.clone(), content.clone());
                content
            };

            let (source, source_type) = prepare_module_source(&module_specifier, bytes)?;

            Ok(ModuleSource::new(
                source_type,
                ModuleSourceCode::Bytes(source.into_boxed_slice().into()),
                &module_specifier,
                None,
            ))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deno_core::{ModuleType, RequestedModuleType};

    #[tokio::test]
    async fn test_memory_loader_wasm() {
        let wasm_bytes = b"\x00\x61\x73\x6d\x01\x00\x00\x00".to_vec(); // Minimal valid wasm header
        let specifier = ModuleSpecifier::parse("deis:///test.wasm").unwrap();
        let mut modules = HashMap::new();
        modules.insert(specifier.clone(), wasm_bytes.clone());

        let loader = MemoryModuleLoader::new(modules);

        let response = loader.load(
            &specifier,
            None,
            ModuleLoadOptions {
                is_dynamic_import: false,
                is_synchronous: false,
                requested_module_type: RequestedModuleType::None,
            },
        );
        if let ModuleLoadResponse::Async(future) = response {
            let source = future.await.unwrap();
            assert_eq!(source.module_type, ModuleType::Wasm);
            if let ModuleSourceCode::Bytes(bytes) = source.code {
                assert_eq!(bytes.as_bytes(), &wasm_bytes);
            } else {
                panic!("Expected Bytes");
            }
        } else {
            panic!("Expected Async response");
        }
    }

    #[tokio::test]
    async fn test_db_loader_wasm() {
        let app_id = "test_app".to_string();
        let specifier = ModuleSpecifier::parse("deis:///apps/test_app/module.wasm").unwrap();
        let wasm_bytes = b"\x00\x61\x73\x6d\x01\x00\x00\x00".to_vec();

        let loader =
            DbModuleLoader::new(app_id, specifier.clone(), wasm_bytes.clone(), None).unwrap();

        let response = loader.load(
            &specifier,
            None,
            ModuleLoadOptions {
                is_dynamic_import: false,
                is_synchronous: false,
                requested_module_type: RequestedModuleType::None,
            },
        );
        if let ModuleLoadResponse::Async(future) = response {
            let source = future.await.unwrap();
            assert_eq!(source.module_type, ModuleType::Wasm);
            if let ModuleSourceCode::Bytes(bytes) = source.code {
                assert_eq!(bytes.as_bytes(), &wasm_bytes);
            } else {
                panic!("Expected Bytes");
            }
        } else {
            panic!("Expected Async response");
        }
    }
}
