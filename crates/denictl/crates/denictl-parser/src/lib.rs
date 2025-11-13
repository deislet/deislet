use denictl_core::HandlerInfo;
use swc_common::{sync::Lrc, FileName, SourceMap};
use swc_ecma_ast::*;
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsConfig};
use swc_ecma_visit::{Visit, VisitWith};

pub struct TypeScriptParser {
    source_map: Lrc<SourceMap>,
}

impl TypeScriptParser {
    pub fn new() -> Self {
        Self {
            source_map: Lrc::new(SourceMap::default()),
        }
    }

    pub fn parse(&self, code: &str, filename: &str) -> anyhow::Result<HandlerInfo> {
        let source_file = self
            .source_map
            .new_source_file(FileName::Custom(filename.to_string()), code.to_string());

        let lexer = Lexer::new(
            Syntax::Typescript(TsConfig {
                tsx: false,
                decorators: false,
                ..Default::default()
            }),
            EsVersion::Es2022,
            StringInput::from(&*source_file),
            None,
        );

        let mut parser = Parser::new_from(lexer);
        let module = parser
            .parse_module()
            .map_err(|e| anyhow::anyhow!("Parse error: {:?}", e))?;

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

    fn path_to_route(path: &str) -> String {
        // Simple route extraction
        // functions/api/users.ts -> /api/users
        // functions/index.ts -> /
        let route = path
            .trim_start_matches("functions/")
            .trim_end_matches(".ts")
            .trim_end_matches(".js");

        if route == "index" || route.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", route)
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
}

impl Visit for HandlerVisitor {
    fn visit_export_default_decl(&mut self, _: &ExportDefaultDecl) {
        self.has_default_export = true;
    }

    fn visit_export_default_expr(&mut self, _: &ExportDefaultExpr) {
        self.has_default_export = true;
    }

    fn visit_named_export(&mut self, n: &NamedExport) {
        for spec in n.specifiers.iter() {
            if let ExportSpecifier::Named(named) = spec {
                if let ModuleExportName::Ident(ident) = &named.orig {
                    self.named_exports.push(ident.sym.to_string());
                }
            }
        }
    }

    fn visit_import_decl(&mut self, n: &ImportDecl) {
        self.imports.push(n.src.value.to_string());
    }

    fn visit_member_expr(&mut self, n: &MemberExpr) {
        // Detect context.services.kv, context.services.database, etc.
        if let Expr::Member(inner) = &*n.obj {
            if let (Expr::Ident(obj), MemberProp::Ident(prop1)) = (&*inner.obj, &inner.prop) {
                if obj.sym.as_ref() == "context" && prop1.sym.as_ref() == "services" {
                    if let MemberProp::Ident(service) = &n.prop {
                        let service_name = service.sym.to_string();
                        if !self.used_services.contains(&service_name) {
                            self.used_services.push(service_name);
                        }
                    }
                }
            }
        }
    }
}
