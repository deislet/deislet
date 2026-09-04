use deis_build_core::{
    HandlerInfo, ModuleReference, ModuleReferenceKind, ModuleSourcePosition,
    ProviderOnlyGlobalReference,
};
use swc_common::{
    sync::Lrc, FileName, Globals, Mark, SourceMap, Span, Spanned, SyntaxContext, GLOBALS,
};
use swc_ecma_ast::*;
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::{Visit, VisitMutWith, VisitWith};

pub struct TypeScriptParser {
    source_map: Lrc<SourceMap>,
}

impl Default for TypeScriptParser {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeScriptParser {
    pub fn new() -> Self {
        Self {
            source_map: Lrc::new(SourceMap::default()),
        }
    }

    pub fn parse(&self, code: &str, filename: &str) -> anyhow::Result<HandlerInfo> {
        let source_file = self.source_map.new_source_file(
            FileName::Custom(filename.to_string()).into(),
            code.to_string(),
        );

        let lexer = Lexer::new(
            Syntax::Typescript(Default::default()),
            EsVersion::Es2022,
            StringInput::from(&*source_file),
            None,
        );

        let mut parser = Parser::new_from(lexer);
        let mut module = parser
            .parse_module()
            .map_err(|e| anyhow::anyhow!("Parse error: {e:?}"))?;

        let visitor = GLOBALS.set(&Globals::new(), || {
            // Give unresolved globals a stable syntax context before the
            // policy visitor runs. A spelling such as `WebSocket` can also be
            // a perfectly portable local class or import; only the unresolved
            // global is the provider API that EC-STREAM excludes.
            let unresolved_mark = Mark::new();
            let top_level_mark = Mark::new();
            module.visit_mut_with(&mut resolver(unresolved_mark, top_level_mark, true));
            let unresolved_context = SyntaxContext::empty().apply_mark(unresolved_mark);
            let mut visitor = HandlerVisitor::new(self.source_map.clone(), unresolved_context);
            module.visit_with(&mut visitor);
            visitor
        });

        Ok(HandlerInfo {
            path: filename.to_string(),
            route: Self::path_to_route(filename),
            has_default_export: visitor.has_default_export,
            named_exports: visitor.named_exports,
            used_services: visitor.used_services,
            imports: visitor.imports,
            module_references: visitor.module_references,
            non_literal_module_loads: visitor.non_literal_module_loads,
            non_literal_module_load_positions: visitor.non_literal_module_load_positions,
            provider_only_global_references: visitor.provider_only_global_references,
        })
    }

    /// A rough route for the handler being parsed, used only in diagnostics.
    ///
    /// Each affix comes off once. `trim_start_matches` / `trim_end_matches`
    /// take theirs off as many times as they appear, so
    /// `functions/functions/x.ts` lost both directories and `report.ts.ts` lost
    /// both suffixes - the same repeated-prefix mistake that made a file under
    /// a directory called `src` into the application's queue consumer.
    ///
    /// The route the build actually uses is `deis_build_core::routes`, which
    /// this deliberately does not try to reproduce: it knows nothing about
    /// `[id]` or `[[catch]]`, and a second definition of the routing rule is
    /// how an app ends up answering on one target and 404ing on another.
    fn path_to_route(path: &str) -> String {
        // Simple route extraction
        // functions/api/users.ts -> /api/users
        // functions/index.ts -> /
        let route = path.strip_prefix("functions/").unwrap_or(path);
        let route = route
            .strip_suffix(".ts")
            .or_else(|| route.strip_suffix(".js"))
            .unwrap_or(route);

        if route == "index" || route.is_empty() {
            "/".to_string()
        } else {
            format!("/{route}")
        }
    }
}

struct HandlerVisitor {
    source_map: Lrc<SourceMap>,
    unresolved_context: SyntaxContext,
    has_default_export: bool,
    named_exports: Vec<String>,
    used_services: Vec<String>,
    imports: Vec<String>,
    module_references: Vec<ModuleReference>,
    non_literal_module_loads: usize,
    non_literal_module_load_positions: Vec<ModuleSourcePosition>,
    provider_only_global_references: Vec<ProviderOnlyGlobalReference>,
}

impl HandlerVisitor {
    fn new(source_map: Lrc<SourceMap>, unresolved_context: SyntaxContext) -> Self {
        Self {
            source_map,
            unresolved_context,
            has_default_export: false,
            named_exports: Vec::new(),
            used_services: Vec::new(),
            imports: Vec::new(),
            module_references: Vec::new(),
            non_literal_module_loads: 0,
            non_literal_module_load_positions: Vec::new(),
            provider_only_global_references: Vec::new(),
        }
    }

    fn check_service_usage(&mut self, expr: &Expr) {
        // Check if this expression is context.services.X
        if let Expr::Member(member_expr) = expr {
            // Try to match pattern: context.services.X
            if let MemberProp::Ident(service_prop) = &member_expr.prop {
                if let Expr::Member(inner_member) = &*member_expr.obj {
                    if let MemberProp::Ident(services_ident) = &inner_member.prop {
                        if services_ident.sym.as_ref() == "services" {
                            if let Expr::Ident(context_ident) = &*inner_member.obj {
                                if context_ident.sym.as_ref() == "context" {
                                    let service_name = service_prop.sym.to_string();
                                    if !self.used_services.contains(&service_name) {
                                        self.used_services.push(service_name);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn record_import(&mut self, specifier: &str) {
        if !self.imports.iter().any(|existing| existing == specifier) {
            self.imports.push(specifier.to_string());
        }
    }

    fn record_module_reference(
        &mut self,
        specifier: &str,
        requested_exports: Vec<String>,
        loads_namespace: bool,
        kind: ModuleReferenceKind,
        span: Span,
    ) {
        let location = self.source_map.lookup_char_pos(span.lo());
        self.module_references.push(ModuleReference {
            specifier: specifier.to_string(),
            requested_exports,
            loads_namespace,
            kind,
            line: location.line,
            column: location.col_display + 1,
        });
    }

    fn record_provider_only_global(&mut self, name: &str, span: Span) {
        if !matches!(name, "WebSocket" | "WebSocketPair") {
            return;
        }
        let location = self.source_map.lookup_char_pos(span.lo());
        let reference = ProviderOnlyGlobalReference {
            name: name.to_string(),
            line: location.line,
            column: location.col_display + 1,
        };
        if !self
            .provider_only_global_references
            .iter()
            .any(|existing| existing == &reference)
        {
            self.provider_only_global_references.push(reference);
        }
    }

    fn is_global_object(&self, expression: &Expr) -> bool {
        match expression {
            Expr::Ident(ident) => {
                matches!(ident.sym.as_ref(), "globalThis" | "self")
                    && ident.ctxt == self.unresolved_context
            }
            Expr::Paren(paren) => self.is_global_object(&paren.expr),
            _ => false,
        }
    }

    fn record_provider_only_property_name(&mut self, property: &PropName) {
        match property {
            PropName::Ident(ident) => {
                self.record_provider_only_global(ident.sym.as_ref(), ident.span)
            }
            PropName::Str(value) => {
                self.record_provider_only_global(&value.value.to_string_lossy(), value.span)
            }
            PropName::Computed(computed) => {
                if let Expr::Lit(Lit::Str(value)) = &*computed.expr {
                    self.record_provider_only_global(&value.value.to_string_lossy(), value.span);
                }
            }
            PropName::Num(_) | PropName::BigInt(_) => {}
        }
    }
}

/// The text of an exported name, whether it was written as an identifier or as
/// a string literal (`export { x as "with-a-dash" }`).
fn export_name(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::Ident(ident) => ident.sym.to_string(),
        ModuleExportName::Str(literal) => literal.value.to_string_lossy().into_owned(),
    }
}

impl Visit for HandlerVisitor {
    fn visit_export_default_decl(&mut self, declaration: &ExportDefaultDecl) {
        self.has_default_export = true;
        declaration.visit_children_with(self);
    }

    fn visit_export_default_expr(&mut self, expression: &ExportDefaultExpr) {
        self.has_default_export = true;
        expression.visit_children_with(self);
    }

    fn visit_export_decl(&mut self, n: &ExportDecl) {
        // Handle: export const/let/function/class
        match &n.decl {
            Decl::Var(var_decl) => {
                for decl in var_decl.decls.iter() {
                    if let Pat::Ident(ident) = &decl.name {
                        self.named_exports.push(ident.id.sym.to_string());
                    }
                }
            }
            Decl::Fn(fn_decl) => {
                self.named_exports.push(fn_decl.ident.sym.to_string());
            }
            Decl::Class(class_decl) => {
                self.named_exports.push(class_decl.ident.sym.to_string());
            }
            _ => {}
        }
        n.visit_children_with(self);
    }

    fn visit_named_export(&mut self, n: &NamedExport) {
        // `export { foo, bar }`, `export { handler as default }`,
        // `export { default } from "./elsewhere"`.
        //
        // The name that matters is the one the module is exported *under*, not
        // the local binding: `export { handler as default }` is a default
        // export, and this used to record it as a named export called
        // `handler` with `has_default_export` still false. That was invisible
        // while a missing default export was not an error; the moment it
        // became one it would have failed a build over an idiomatic file that
        // is perfectly correct. `export { default as thing }` is the mirror
        // image and has to come out the other way round: a named export called
        // `thing`, and no default.
        for spec in n.specifiers.iter() {
            let name = match spec {
                ExportSpecifier::Named(named) => {
                    export_name(named.exported.as_ref().unwrap_or(&named.orig))
                }
                // `export * as ns from "./x"` publishes one name and no
                // default; `export * from "./x"` publishes whatever the other
                // module does, which this file cannot know.
                ExportSpecifier::Namespace(namespace) => export_name(&namespace.name),
                ExportSpecifier::Default(_) => "default".to_string(),
            };
            if name == "default" {
                self.has_default_export = true;
            } else {
                self.named_exports.push(name);
            }
        }
        if let Some(source) = &n.src {
            let source = source.value.to_string_lossy();
            self.record_import(&source);
            if !n.type_only {
                let mut requested_exports = Vec::new();
                let mut loads_namespace = false;
                for specifier in &n.specifiers {
                    match specifier {
                        ExportSpecifier::Named(named) if !named.is_type_only => {
                            requested_exports.push(export_name(&named.orig));
                        }
                        ExportSpecifier::Namespace(_) => loads_namespace = true,
                        ExportSpecifier::Default(_) => {
                            requested_exports.push("default".to_string());
                        }
                        ExportSpecifier::Named(_) => {}
                    }
                }
                if !requested_exports.is_empty() || loads_namespace || n.specifiers.is_empty() {
                    self.record_module_reference(
                        &source,
                        requested_exports,
                        loads_namespace,
                        ModuleReferenceKind::ReExport,
                        n.span(),
                    );
                }
            }
        }
    }

    fn visit_import_decl(&mut self, n: &ImportDecl) {
        let source = n.src.value.to_string_lossy();
        self.record_import(&source);
        if n.type_only {
            return;
        }

        let mut requested_exports = Vec::new();
        let mut loads_namespace = false;
        for specifier in &n.specifiers {
            match specifier {
                ImportSpecifier::Named(named) if !named.is_type_only => {
                    requested_exports.push(
                        named
                            .imported
                            .as_ref()
                            .map(export_name)
                            .unwrap_or_else(|| named.local.sym.to_string()),
                    );
                }
                ImportSpecifier::Default(_) => requested_exports.push("default".to_string()),
                ImportSpecifier::Namespace(_) => loads_namespace = true,
                ImportSpecifier::Named(_) => {}
            }
        }
        if !requested_exports.is_empty() || loads_namespace || n.specifiers.is_empty() {
            self.record_module_reference(
                &source,
                requested_exports,
                loads_namespace,
                ModuleReferenceKind::StaticImport,
                n.span(),
            );
        }
    }

    fn visit_export_all(&mut self, n: &ExportAll) {
        let source = n.src.value.to_string_lossy();
        self.record_import(&source);
        if !n.type_only {
            self.record_module_reference(
                &source,
                Vec::new(),
                true,
                ModuleReferenceKind::ReExport,
                n.span(),
            );
        }
    }

    fn visit_expr(&mut self, n: &Expr) {
        // Check any expression for service usage
        self.check_service_usage(n);

        // WebSocket termination is not in the current provider intersection.
        // Observe runtime expressions, including computed property access,
        // without scanning comments, string contents or erased TS types.
        match n {
            Expr::Ident(ident) if ident.ctxt == self.unresolved_context => {
                self.record_provider_only_global(ident.sym.as_ref(), ident.span);
            }
            Expr::Member(member) if self.is_global_object(&member.obj) => match &member.prop {
                MemberProp::Ident(ident) => {
                    self.record_provider_only_global(ident.sym.as_ref(), ident.span);
                }
                MemberProp::Computed(computed) => {
                    if let Expr::Lit(Lit::Str(value)) = &*computed.expr {
                        self.record_provider_only_global(
                            &value.value.to_string_lossy(),
                            value.span,
                        );
                    }
                }
                MemberProp::PrivateName(_) => {}
            },
            _ => {}
        }

        // These literal forms resolve files just as a static import does.
        // They belong in the staged module graph as well; otherwise a build
        // can ship its eager path and leave its lazy or CommonJS path behind.
        if let Expr::Call(call) = n {
            let dynamic_import = matches!(call.callee, Callee::Import(_));
            let require = matches!(
                &call.callee,
                Callee::Expr(callee)
                    if matches!(&**callee, Expr::Ident(ident) if ident.sym == *"require")
            );
            let imports_module = dynamic_import || require;
            if imports_module {
                if let Some(argument) = call.args.first() {
                    if let Expr::Lit(Lit::Str(specifier)) = &*argument.expr {
                        let specifier = specifier.value.to_string_lossy();
                        self.record_import(&specifier);
                        self.record_module_reference(
                            &specifier,
                            Vec::new(),
                            true,
                            if dynamic_import {
                                ModuleReferenceKind::DynamicImport
                            } else {
                                ModuleReferenceKind::Require
                            },
                            call.span(),
                        );
                    } else {
                        self.non_literal_module_loads += 1;
                        let location = self.source_map.lookup_char_pos(call.span().lo());
                        self.non_literal_module_load_positions
                            .push(ModuleSourcePosition {
                                line: location.line,
                                column: location.col_display + 1,
                            });
                    }
                } else {
                    self.non_literal_module_loads += 1;
                    let location = self.source_map.lookup_char_pos(call.span().lo());
                    self.non_literal_module_load_positions
                        .push(ModuleSourcePosition {
                            line: location.line,
                            column: location.col_display + 1,
                        });
                }
            }
        }

        // Continue visiting children
        n.visit_children_with(self);
    }

    fn visit_var_declarator(&mut self, n: &VarDeclarator) {
        // Destructuring reads the global property without forming a MemberExpr.
        if let (Pat::Object(pattern), Some(initializer)) = (&n.name, &n.init) {
            if self.is_global_object(initializer) {
                for property in &pattern.props {
                    match property {
                        ObjectPatProp::KeyValue(key_value) => {
                            self.record_provider_only_property_name(&key_value.key);
                        }
                        ObjectPatProp::Assign(assign) => self.record_provider_only_global(
                            assign.key.id.sym.as_ref(),
                            assign.key.id.span,
                        ),
                        ObjectPatProp::Rest(_) => {}
                    }
                }
            }
        }
        n.visit_children_with(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_default_export() {
        let parser = TypeScriptParser::new();
        let code = r#"
export default async function handler(context) {
    return new Response("Hello");
}
"#;

        let result = parser.parse(code, "functions/api/hello.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(info.has_default_export);
        assert_eq!(info.route, "/api/hello");
        assert_eq!(info.named_exports.len(), 0);
    }

    /// `export { handler as default }` is a default export, and
    /// `export { default } from "./elsewhere"` is one too. Both used to be
    /// recorded as named exports with no default, which - now that a missing
    /// default export stops a build - would have refused two correct files.
    #[test]
    fn a_default_written_as_a_named_specifier_is_still_a_default() {
        let parser = TypeScriptParser::new();
        for code in [
            "async function handler(c) { return new Response(\"x\"); }\nexport { handler as default };\n",
            "export { default } from \"./elsewhere\";\n",
            "export { default, helper } from \"./elsewhere\";\n",
        ] {
            let info = parser.parse(code, "functions/index.ts").expect("parses");
            assert!(info.has_default_export, "{code}");
            assert!(!info.named_exports.contains(&"default".to_string()), "{code}");
        }
    }

    /// And the mirror image: re-exporting somebody else's default under
    /// another name leaves this file without one.
    #[test]
    fn a_default_re_exported_under_another_name_is_not_this_files_default() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "export { default as report } from \"./elsewhere\";\n",
                "functions/index.ts",
            )
            .expect("parses");
        assert!(!info.has_default_export);
        assert_eq!(info.named_exports, vec!["report".to_string()]);
    }

    #[test]
    fn test_parse_named_exports() {
        let parser = TypeScriptParser::new();
        let code = r#"
export const config = { runtime: 'edge' };
export const handler = async (ctx) => { return new Response("OK"); };
export function middleware() { return null; }
"#;

        let result = parser.parse(code, "functions/api/test.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(!info.has_default_export);
        assert!(info.named_exports.contains(&"config".to_string()));
        assert!(info.named_exports.contains(&"handler".to_string()));
        assert!(info.named_exports.contains(&"middleware".to_string()));
    }

    #[test]
    fn test_parse_default_export_arrow_function() {
        let parser = TypeScriptParser::new();
        let code = r#"
export default async (context) => {
    const data = await fetch("https://api.example.com");
    return new Response(data);
}
"#;

        let result = parser.parse(code, "functions/index.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(info.has_default_export);
        assert_eq!(info.route, "/");
    }

    // Not covered here: service detection from the AST. Exports, imports and
    // routing are, and those are what the build depends on - which services an
    // application uses is read from `.config.json`, not inferred from its code,
    // so nothing downstream relies on the detection these tests would cover.

    #[test]
    fn test_detect_imports() {
        let parser = TypeScriptParser::new();
        let code = r#"
import { z } from 'zod';
import type { Context } from '@deis/types';
import * as utils from '../utils';

export default async function handler(context: Context) {
    return new Response("OK");
}
"#;

        let result = parser.parse(code, "functions/api/validate.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert_eq!(info.imports.len(), 3);
        assert!(info.imports.contains(&"zod".to_string()));
        assert!(info.imports.contains(&"@deis/types".to_string()));
        assert!(info.imports.contains(&"../utils".to_string()));
    }

    #[test]
    fn every_literal_module_edge_is_recorded_once() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                r#"
import value from './static.js';
export { value as named } from './reexport.js';
export * from './all.js';
const lazy = import('./lazy.js');
const legacy = require('./legacy.cjs');
export default async function handler() { return new Response(String(value)); }
"#,
                "functions/index.js",
            )
            .expect("module parses");

        assert_eq!(
            info.imports,
            vec![
                "./static.js",
                "./reexport.js",
                "./all.js",
                "./lazy.js",
                "./legacy.cjs",
            ]
        );
        assert_eq!(info.non_literal_module_loads, 0);
    }

    #[test]
    fn module_edges_preserve_the_export_surface_and_source_position() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "import crypto, { createHash as hash, type KeyObject } from 'node:crypto';\n\
                 import * as path from 'node:path';\n\
                 import 'node:diagnostics_channel';\n\
                 export { EventEmitter as Events } from 'node:events';\n\
                 export * from 'node:buffer';\n\
                 const timers = require('node:timers');\n\
                 const streams = import('node:stream');\n\
                 export default async function handler() { return new Response(String(hash)); }\n",
                "functions/index.ts",
            )
            .expect("module parses");

        assert_eq!(info.module_references.len(), 7);
        assert_eq!(
            info.module_references[0].requested_exports,
            vec!["default", "createHash"]
        );
        assert!(!info.module_references[0].loads_namespace);
        assert_eq!(info.module_references[0].line, 1);
        assert_eq!(info.module_references[0].column, 1);
        assert!(info.module_references[1].loads_namespace);
        assert!(info.module_references[2].requested_exports.is_empty());
        assert!(!info.module_references[2].loads_namespace);
        assert_eq!(
            info.module_references[3].requested_exports,
            vec!["EventEmitter"]
        );
        assert!(info.module_references[4].loads_namespace);
        assert_eq!(info.module_references[5].kind, ModuleReferenceKind::Require);
        assert_eq!(
            info.module_references[6].kind,
            ModuleReferenceKind::DynamicImport
        );
    }

    #[test]
    fn type_only_edges_do_not_become_runtime_surface_claims() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "import type { KeyObject } from 'node:crypto';\n\
                 export type { EventEmitter } from 'node:events';\n\
                 export default function handler() {}\n",
                "functions/index.ts",
            )
            .expect("module parses");

        assert!(info.module_references.is_empty());
        assert_eq!(info.imports, vec!["node:crypto", "node:events"]);
    }

    #[test]
    fn computed_module_loads_are_counted_for_a_fail_closed_build() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "export default async function handler(name) { await import(name); return require(name); }",
                "functions/index.js",
            )
            .expect("module parses");
        assert_eq!(info.non_literal_module_loads, 2);
        assert_eq!(info.non_literal_module_load_positions.len(), 2);
        assert_eq!(info.non_literal_module_load_positions[0].line, 1);
    }

    #[test]
    fn provider_only_websocket_globals_are_ast_references_not_text_matches() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "type WebSocket = string;\n\
                 const harmless = 'WebSocketPair'; // new WebSocket('ignored')\n\
                 const one = new WebSocket(url);\n\
                 const two = globalThis['WebSocketPair'];\n\
                 export default function handler() { return new Response(harmless + one + two); }\n",
                "functions/index.ts",
            )
            .expect("module parses");
        assert_eq!(info.provider_only_global_references.len(), 2);
        assert_eq!(info.provider_only_global_references[0].name, "WebSocket");
        assert_eq!(info.provider_only_global_references[0].line, 3);
        assert!(info.provider_only_global_references[0].column > 0);
        assert_eq!(
            info.provider_only_global_references[1].name,
            "WebSocketPair"
        );
        assert_eq!(info.provider_only_global_references[1].line, 4);
        assert!(info.provider_only_global_references[1].column > 0);
    }

    #[test]
    fn local_websocket_names_are_not_provider_globals() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "import { WebSocketPair } from './portable.ts';\n\
                 class WebSocket { constructor(_url: string) {} }\n\
                 export default function handler() {\n\
                   return new Response(String(new WebSocket('local')) + WebSocketPair);\n\
                 }\n",
                "functions/index.ts",
            )
            .expect("module parses");
        assert!(info.provider_only_global_references.is_empty());
    }

    #[test]
    fn global_object_destructuring_is_a_provider_reference() {
        let parser = TypeScriptParser::new();
        let info = parser
            .parse(
                "const { WebSocket: Socket, ['WebSocketPair']: Pair } = globalThis;\n\
                 export default function handler() { return new Response(String(Socket) + Pair); }\n",
                "functions/index.ts",
            )
            .expect("module parses");
        assert_eq!(info.provider_only_global_references.len(), 2);
        assert_eq!(info.provider_only_global_references[0].name, "WebSocket");
        assert_eq!(
            info.provider_only_global_references[1].name,
            "WebSocketPair"
        );
    }

    #[test]
    fn test_path_to_route_index() {
        let route1 = TypeScriptParser::path_to_route("functions/index.ts");
        assert_eq!(route1, "/");

        let route2 = TypeScriptParser::path_to_route("functions/index.js");
        assert_eq!(route2, "/");
    }

    #[test]
    fn test_path_to_route_simple() {
        let route1 = TypeScriptParser::path_to_route("functions/api/users.ts");
        assert_eq!(route1, "/api/users");

        let route2 = TypeScriptParser::path_to_route("functions/hello.js");
        assert_eq!(route2, "/hello");
    }

    #[test]
    fn test_path_to_route_nested() {
        let route1 = TypeScriptParser::path_to_route("functions/api/v1/users.ts");
        assert_eq!(route1, "/api/v1/users");

        let route2 = TypeScriptParser::path_to_route("functions/admin/dashboard/stats.js");
        assert_eq!(route2, "/admin/dashboard/stats");
    }

    #[test]
    fn test_path_to_route_dynamic_params() {
        let route1 = TypeScriptParser::path_to_route("functions/api/users/[id].ts");
        assert_eq!(route1, "/api/users/[id]");

        let route2 = TypeScriptParser::path_to_route("functions/posts/[slug]/comments.js");
        assert_eq!(route2, "/posts/[slug]/comments");
    }

    #[test]
    fn test_parse_typescript_with_types() {
        let parser = TypeScriptParser::new();
        let code = r#"
interface User {
    id: number;
    name: string;
}

export default async function handler(context: Context): Promise<Response> {
    const user: User = { id: 1, name: "Alice" };
    return Response.json(user);
}
"#;

        let result = parser.parse(code, "functions/api/users.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(info.has_default_export);
    }

    #[test]
    fn test_parse_mixed_exports() {
        let parser = TypeScriptParser::new();
        let code = r#"
export const config = { runtime: 'edge' };

export default async function handler(context) {
    return new Response("Hello");
}

export const metadata = { version: '1.0.0' };
"#;

        let result = parser.parse(code, "functions/api/hello.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(info.has_default_export);
        assert!(info.named_exports.contains(&"config".to_string()));
        assert!(info.named_exports.contains(&"metadata".to_string()));
    }

    #[test]
    fn test_parse_no_exports() {
        let parser = TypeScriptParser::new();
        let code = r#"
function helperFunction() {
    return "helper";
}

const data = { key: "value" };
"#;

        let result = parser.parse(code, "functions/helpers.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(!info.has_default_export);
        assert_eq!(info.named_exports.len(), 0);
    }

    #[test]
    fn test_parse_invalid_syntax() {
        let parser = TypeScriptParser::new();
        let code = r#"
export default async function handler(context {
    // Missing closing parenthesis
    return new Response("Error");
}
"#;

        let result = parser.parse(code, "functions/broken.ts");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_jsx_like_syntax() {
        let parser = TypeScriptParser::new();
        let code = r#"
export default async function handler(context) {
    const html = "<div>Hello World</div>";
    return new Response(html, {
        headers: { "content-type": "text/html" }
    });
}
"#;

        let result = parser.parse(code, "functions/page.ts");
        assert!(result.is_ok());

        let info = result.unwrap();
        assert!(info.has_default_export);
    }
}
