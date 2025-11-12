use denictl_core::*;
use swc_common::{sync::Lrc, FileName, SourceMap};
use swc_ecma_ast::*;
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsConfig};
use swc_ecma_visit::{Visit, VisitWith};

pub struct Validator {
    source_map: Lrc<SourceMap>,
}

impl Validator {
    pub fn new() -> Self {
        Self {
            source_map: Lrc::new(SourceMap::default()),
        }
    }

    pub fn validate(
        &self,
        code: &str,
        handler_info: &HandlerInfo,
        _config: &ProjectConfig,
        _strict: bool,
        result: &mut ValidationResult,
    ) -> anyhow::Result<()> {
        // Check for exports
        if !handler_info.has_default_export && handler_info.named_exports.is_empty() {
            result.add_error(ValidationError {
                file: handler_info.path.clone(),
                line: 1,
                column: 1,
                message: "Handler must have a default export or named exports".to_string(),
                code: "NO_EXPORTS".to_string(),
                severity: Severity::Error,
                suggestion: Some(
                    "Add: export default async function handler(context: Context): Promise<Response>"
                        .to_string(),
                ),
            });
        }

        // Parse for deeper validation
        let source_file = self.source_map.new_source_file(
            Lrc::new(FileName::Custom(handler_info.path.clone())),
            code.to_string(),
        );

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
        if let Ok(module) = parser.parse_module() {
            let mut visitor = ValidationVisitor::new(&handler_info.path, result);
            module.visit_with(&mut visitor);
        }

        Ok(())
    }
}

struct ValidationVisitor<'a> {
    file: &'a str,
    result: &'a mut ValidationResult,
}

impl<'a> ValidationVisitor<'a> {
    fn new(file: &'a str, result: &'a mut ValidationResult) -> Self {
        Self { file, result }
    }

    fn add_error(&mut self, line: usize, message: String, code: String, suggestion: Option<String>) {
        self.result.add_error(ValidationError {
            file: self.file.to_string(),
            line,
            column: 1,
            message,
            code,
            severity: Severity::Error,
            suggestion,
        });
    }
}

impl<'a> Visit for ValidationVisitor<'a> {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        // Check for typeof checks on platform globals
        let expr_str = format!("{:?}", n);

        if expr_str.contains("typeof Deno")
            || expr_str.contains("typeof Cloudflare")
            || expr_str.contains("typeof EdgeRuntime")
        {
            self.add_error(
                1, // Line number would need span information
                "Platform-specific code detected: You cannot check for platform type".to_string(),
                "PLATFORM_DETECTION".to_string(),
                Some("Use unified interfaces from context.services instead".to_string()),
            );
        }
    }

    fn visit_member_expr(&mut self, n: &MemberExpr) {
        // Check for context.raw access
        if let Expr::Member(inner) = &*n.obj {
            if let (Expr::Ident(obj), MemberProp::Ident(prop)) = (&*inner.obj, &inner.prop) {
                if obj.sym.as_ref() == "context" && prop.sym.as_ref() == "raw" {
                    self.add_error(
                        1,
                        "Direct access to context.raw is prohibited".to_string(),
                        "RAW_ACCESS".to_string(),
                        Some("Use unified context interfaces instead".to_string()),
                    );
                }
            }
        }

        // Check for platform-specific APIs
        if let Expr::Ident(obj) = &*n.obj {
            if obj.sym.as_ref() == "Deno" || obj.sym.as_ref() == "process" {
                if let MemberProp::Ident(prop) = &n.prop {
                    if prop.sym.as_ref() == "env" {
                        self.add_error(
                            1,
                            format!("Platform-specific API detected: {}.env", obj.sym),
                            "PLATFORM_API".to_string(),
                            Some("Use context.env or context.services instead".to_string()),
                        );
                    }
                }
            }
        }
    }
}
