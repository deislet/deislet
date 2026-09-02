use deis_build_core::HandlerInfo;
use swc_common::{sync::Lrc, FileName, SourceMap};
use swc_ecma_ast::*;
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax};
use swc_ecma_visit::{Visit, VisitWith};

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
        let module = parser
            .parse_module()
            .map_err(|e| anyhow::anyhow!("Parse error: {e:?}"))?;

        let mut visitor = HandlerVisitor::new();
        module.visit_with(&mut visitor);

        Ok(HandlerInfo {
            path: filename.to_string(),
            route: Self::path_to_route(filename),
            has_default_export: visitor.has_default_export,
            named_exports: visitor.named_exports,
            used_services: visitor.used_services,
            imports: visitor.imports,
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
    has_default_export: bool,
    named_exports: Vec<String>,
    used_services: Vec<String>,
    imports: Vec<String>,
}

impl HandlerVisitor {
    fn new() -> Self {
        Self {
            has_default_export: false,
            named_exports: Vec::new(),
            used_services: Vec::new(),
            imports: Vec::new(),
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
    fn visit_export_default_decl(&mut self, _: &ExportDefaultDecl) {
        self.has_default_export = true;
    }

    fn visit_export_default_expr(&mut self, _: &ExportDefaultExpr) {
        self.has_default_export = true;
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
    }

    fn visit_import_decl(&mut self, n: &ImportDecl) {
        self.imports.push(n.src.value.to_string_lossy().to_string());
    }

    fn visit_expr(&mut self, n: &Expr) {
        // Check any expression for service usage
        self.check_service_usage(n);

        // Continue visiting children
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
