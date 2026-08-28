use crate::app_repo::AppRepo;
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
        deno_core::resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&deno_core::ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let module_specifier = module_specifier.clone();
        let bytes_opt = self.modules.get(&module_specifier).cloned();

        ModuleLoadResponse::Async(Box::pin(async move {
            let bytes = bytes_opt.ok_or_else(|| {
                JsErrorBox::generic(format!("Module not found: {}", module_specifier))
            })?;

            let media_type = MediaType::from_specifier(&module_specifier);

            let (source, source_type) = if media_type == MediaType::Wasm {
                (bytes, ModuleType::Wasm)
            } else if media_type == MediaType::TypeScript
                || media_type == MediaType::Tsx
                || media_type == MediaType::Jsx
            {
                // Transpilation requires string
                let text =
                    String::from_utf8(bytes).map_err(|e| JsErrorBox::generic(e.to_string()))?;

                let parsed = deno_ast::parse_module(ParseParams {
                    specifier: module_specifier.clone(),
                    text: text.into(),
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

                (
                    transpiled.into_source().text.into_bytes(),
                    ModuleType::JavaScript,
                )
            } else {
                (bytes, ModuleType::JavaScript)
            };

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
        let maybe_path = self.module_path(&module_specifier);

        ModuleLoadResponse::Async(Box::pin(async move {
            let cached = { modules.read().await.get(&module_specifier).cloned() };
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

            let media_type = MediaType::from_specifier(&module_specifier);

            let (source, source_type) = if media_type == MediaType::Wasm {
                (bytes, ModuleType::Wasm)
            } else if media_type == MediaType::TypeScript
                || media_type == MediaType::Tsx
                || media_type == MediaType::Jsx
            {
                let text =
                    String::from_utf8(bytes).map_err(|e| JsErrorBox::generic(e.to_string()))?;

                let parsed = deno_ast::parse_module(ParseParams {
                    specifier: module_specifier.clone(),
                    text: text.into(),
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

                (
                    transpiled.into_source().text.into_bytes(),
                    ModuleType::JavaScript,
                )
            } else {
                (bytes, ModuleType::JavaScript)
            };

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
