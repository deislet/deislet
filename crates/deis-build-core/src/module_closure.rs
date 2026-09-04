//! Build-time closure for runtime module edges.

use std::borrow::Cow;

use rolldown::plugin::{HookResolveIdArgs, HookResolveIdReturn, HookUsage, Plugin, PluginContext};

/// Return a sanitized class for a module spelling that delegates runtime
/// resolution to a provider or host. Never return the original spelling:
/// forbidden URLs can contain credentials.
pub fn external_runtime_module_kind(specifier: &str) -> Option<String> {
    if specifier.starts_with('/') {
        return Some("absolute-path".to_string());
    }
    let (scheme, _) = specifier.split_once(':')?;
    let mut characters = scheme.chars();
    if !characters
        .next()
        .is_some_and(|value| value.is_ascii_alphabetic())
        || !characters
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '+' | '-' | '.'))
    {
        return None;
    }
    if scheme.eq_ignore_ascii_case("node") {
        None
    } else {
        Some(format!("{}-scheme", scheme.to_ascii_lowercase()))
    }
}

#[derive(Debug, Default)]
pub(crate) struct ModuleClosurePlugin;

impl Plugin for ModuleClosurePlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("edge-canon-module-closure")
    }

    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        // These are private edges emitted by the Node façade plugin, never
        // accepted from application source by the compiler policy gate.
        if args.specifier == super::provider_node_facade::NODE_RUNTIME_SPECIFIER
            || args.specifier.starts_with("edge-canon-facade:")
            || args.specifier.starts_with("edge-canon-native:")
        {
            return Ok(None);
        }
        if args.importer.is_some() {
            if let Some(kind) = external_runtime_module_kind(args.specifier) {
                anyhow::bail!(
                    "EC_ARTIFACT_MODULE_EXTERNAL: {kind} runtime module edge is forbidden; the canonical artifact must carry its complete application and dependency graph"
                );
            }
        }
        Ok(None)
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_external_spellings_without_returning_the_secret_url() {
        for (specifier, expected) in [
            ("https://user:secret@example.invalid/x.js", "https-scheme"),
            ("data:text/javascript,export default 1", "data-scheme"),
            ("npm:package@1", "npm-scheme"),
            ("jsr:@scope/package@1", "jsr-scheme"),
            ("file:///host/x.js", "file-scheme"),
            ("/host/x.js", "absolute-path"),
        ] {
            assert_eq!(
                external_runtime_module_kind(specifier).as_deref(),
                Some(expected)
            );
        }
        for portable in ["./local.js", "../local.js", "package", "node:buffer"] {
            assert_eq!(external_runtime_module_kind(portable), None);
        }
    }
}
